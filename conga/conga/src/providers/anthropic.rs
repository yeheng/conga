//! Anthropic native messages API provider.
//!
//! Uses the same SSE download transport as
//! [`crate::providers::openai_compat`] (see [`crate::providers::sse::download_sse`]);
//! only the body shape and chunk parsing differ: `system` is a top-level
//! field, tools use `input_schema`, and stream deltas arrive as
//! `content_block_delta` events with `text_delta` / `input_json_delta`.

use std::pin::Pin;

use crate::cancel::CancelSignal;

use futures_util::Stream;
use serde_json::json;

use super::collect_text;
use crate::types::context::{ModelSpec, StreamChunk, StreamFn};
use crate::types::message::{AgentMessage, ContentBlock, StopReason};
use crate::types::tool::ToolDefinition;

/// Anthropic native messages provider.
#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.anthropic.com/v1".into(),
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

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl StreamFn for AnthropicProvider {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        signal: Option<CancelSignal>,
    ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let body = build_request_body(model, messages, system_prompt, tools);
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));

        tracing::debug!(url = %url, model = %model.id, "anthropic request");
        tracing::debug!(
            request_body = %serde_json::to_string(&body).unwrap_or_default(),
            "anthropic request body"
        );

        let api_key = self.api_key.clone();
        crate::providers::sse::download_sse(
            "anthropic",
            self.client.clone(),
            url,
            move |req| {
                req.header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01")
            },
            body,
            signal,
            parse_anthropic_chunk,
        )
    }
}

fn build_request_body(
    model: &ModelSpec,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut msgs = fold_same_role(messages.iter().filter_map(convert_message).collect());
    // Third breakpoint on the FINAL message: everything before it (tools +
    // system + the entire prior history) is written to the provider cache
    // this request and read back on every subsequent one. The final
    // message is the only uncached bytes — it becomes prefix on the next
    // request. A breakpoint any earlier (e.g. the second-to-last message)
    // would leave the newest exchange permanently uncached: written, never
    // read. Three breakpoints total, inside Anthropic's 4-mark limit.
    if let Some(last) = msgs.last_mut() {
        if let Some(block) = last
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
            .and_then(|blocks| blocks.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }

    let mut body = json!({
        "model": model.id,
        "max_tokens": model.max_tokens,
        "messages": msgs,
        "stream": true,
    });
    if !system_prompt.is_empty() {
        // Send system as a content-block array with an ephemeral cache_control
        // breakpoint. system + tools form the stable prompt prefix; caching it
        // cuts cached input-token cost ~10x on subsequent turns.
        body["system"] = json!([{
            "type": "text",
            "text": system_prompt,
            "cache_control": {"type": "ephemeral"}
        }]);
    }
    if !tools.is_empty() {
        let mut tools_arr: Vec<serde_json::Value> = tools.iter().map(tool_to_anthropic).collect();
        // Breakpoint on the last tool caches the whole tool definition block.
        if let Some(last) = tools_arr.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = json!(tools_arr);
    }
    body
}

/// Convert one message to an Anthropic message. Anthropic requires user/assistant
/// alternation; tool results go in user messages as `tool_result` blocks.
fn convert_message(msg: &AgentMessage) -> Option<serde_json::Value> {
    match msg {
        AgentMessage::User(u) => {
            let blocks: Vec<_> = u
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    _ => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({"role": "user", "content": blocks}))
            }
        }
        AgentMessage::Assistant(a) => {
            let blocks: Vec<_> = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    ContentBlock::ToolCall { tool_call: tc } => Some(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.function.name,
                        "input": serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                            .unwrap_or(json!({})),
                    })),
                    _ => None,
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(json!({"role": "assistant", "content": blocks}))
            }
        }
        AgentMessage::ToolResult(tr) => Some(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tr.tool_call_id,
                "content": collect_text(&tr.content),
                "is_error": tr.is_error,
            }]
        })),
        AgentMessage::Custom(_) => None,
    }
}

/// Fold adjacent same-role messages into one. Anthropic requires the
/// `messages` array to strictly alternate `user`/`assistant`, but the
/// internal history is fragmentary: every tool result is its own
/// `AgentMessage::ToolResult` (each converts to `role: "user"`), and
/// compaction can prepend a user notice ahead of a kept user message.
/// Concatenating adjacent same-role content block arrays restores the
/// alternation at the wire boundary without touching stored history.
fn fold_same_role(msgs: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut folded: Vec<serde_json::Value> = Vec::with_capacity(msgs.len());
    for msg in msgs {
        if let Some(last) = folded.last_mut() {
            if last["role"] == msg["role"] {
                if let (Some(dst), Some(src)) =
                    (last["content"].as_array_mut(), msg["content"].as_array())
                {
                    dst.extend(src.iter().cloned());
                    continue;
                }
            }
        }
        folded.push(msg);
    }
    folded
}

fn tool_to_anthropic(t: &ToolDefinition) -> serde_json::Value {
    json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.parameters,
    })
}

/// Parse one Anthropic SSE event JSON into zero or more chunks.
pub(crate) fn parse_anthropic_chunk(json_str: &str) -> Vec<StreamChunk> {
    let v: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "content_block_delta" => {
            let delta = match v.get("delta") {
                Some(d) => d,
                None => return vec![],
            };
            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if text.is_empty() {
                        vec![]
                    } else {
                        vec![StreamChunk::TextDelta(text)]
                    }
                }
                "input_json_delta" => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    vec![StreamChunk::ToolCallDelta {
                        index: None,
                        id: String::new(),
                        name: None,
                        args_delta: partial,
                    }]
                }
                "thinking_delta" => {
                    let thinking = delta
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    vec![StreamChunk::ThinkingDelta(thinking)]
                }
                _ => vec![],
            }
        }
        "content_block_start" => {
            // A tool_use block opens here and ONLY here: this event carries
            // the id+name the accumulator keys later input_json_delta
            // continuations against. Text/thinking blocks carry nothing the
            // accumulator needs (their content arrives via deltas).
            let block = match v.get("content_block") {
                Some(b) => b,
                None => return vec![],
            };
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block.get("name").and_then(|n| n.as_str()).map(String::from);
                    vec![StreamChunk::ToolCallDelta {
                        index: None,
                        id,
                        name,
                        args_delta: String::new(),
                    }]
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            // Terminal event: carries the stop reason (`delta.stop_reason`)
            // and the final output-token count. `max_tokens` means the
            // output was cut short — surfaced as MaxTokens so the loop
            // discards partial tool calls instead of executing them with
            // malformed arguments.
            let mut chunks = Vec::new();
            if let Some(reason) = v.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
                let stop = match reason {
                    "max_tokens" => StopReason::MaxTokens,
                    "tool_use" => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                };
                chunks.push(StreamChunk::Stop(stop));
            }
            if let Some(output) = v.pointer("/usage/output_tokens").and_then(|u| u.as_u64()) {
                chunks.push(StreamChunk::Usage {
                    input: 0,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                });
            }
            chunks
        }
        "message_start" => {
            if let Some(input) = v.pointer("/message/usage/input_tokens") {
                let input = input.as_u64().unwrap_or(0);
                // Prompt-cache accounting lives in message_start alongside
                // input_tokens: tokens read from the cache and tokens spent
                // creating cache entries. Absent → 0 (not reported).
                let cache_read = v
                    .pointer("/message/usage/cache_read_input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                let cache_write = v
                    .pointer("/message/usage/cache_creation_input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                vec![StreamChunk::Usage {
                    input,
                    output: 0,
                    cache_read,
                    cache_write,
                }]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(chunks, vec![StreamChunk::TextDelta("Hi".into())]);
    }

    #[test]
    fn parses_input_json_delta() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::ToolCallDelta { args_delta, .. } => {
                assert_eq!(args_delta, "{\"x\":");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_content_block_start_tool_use() {
        // The tool_use id+name arrive ONLY on content_block_start;
        // dropping that event (the pre-fix `_ => vec![]` arm) left
        // accumulated calls with empty id/name, so every Anthropic
        // tool call executed as an unknown tool.
        let json = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            StreamChunk::ToolCallDelta {
                id,
                name,
                args_delta,
                ..
            } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name.as_deref(), Some("read"));
                assert_eq!(args_delta, "");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_message_start_usage() {
        let json = r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(
            chunks,
            vec![StreamChunk::Usage {
                input: 42,
                output: 0,
                cache_read: 0,
                cache_write: 0
            }]
        );
    }

    #[test]
    fn parses_message_start_cache_usage() {
        // message_start also reports prompt-cache stats:
        // cache_read_input_tokens (cache hits) and
        // cache_creation_input_tokens (cache writes). Absent fields must
        // degrade to 0, present ones flow through to the chunk.
        let json = r#"{"type":"message_start","message":{"usage":{
            "input_tokens":100,
            "cache_read_input_tokens":80,
            "cache_creation_input_tokens":15}}}"#;
        let chunks = parse_anthropic_chunk(json);
        assert_eq!(
            chunks,
            vec![StreamChunk::Usage {
                input: 100,
                output: 0,
                cache_read: 80,
                cache_write: 15
            }]
        );
    }

    #[test]
    fn parses_message_delta_stop_reason_max_tokens() {
        // The terminal message_delta carries BOTH the stop reason and the
        // final output-token count; `max_tokens` means the output was cut
        // short. Without this the loop would classify a mid-tool-call
        // truncation as ToolUse and execute it with malformed args.
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":99}}"#;
        assert_eq!(
            parse_anthropic_chunk(json),
            vec![
                StreamChunk::Stop(StopReason::MaxTokens),
                StreamChunk::Usage {
                    input: 0,
                    output: 99,
                    cache_read: 0,
                    cache_write: 0
                },
            ]
        );
    }

    #[test]
    fn parses_message_delta_stop_reason_end_turn_and_tool_use() {
        let end = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#;
        assert_eq!(
            parse_anthropic_chunk(end),
            vec![
                StreamChunk::Stop(StopReason::EndTurn),
                StreamChunk::Usage {
                    input: 0,
                    output: 7,
                    cache_read: 0,
                    cache_write: 0
                },
            ]
        );
        let tools = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}"#;
        assert_eq!(
            parse_anthropic_chunk(tools),
            vec![
                StreamChunk::Stop(StopReason::ToolUse),
                StreamChunk::Usage {
                    input: 0,
                    output: 8,
                    cache_read: 0,
                    cache_write: 0
                },
            ]
        );
    }

    #[test]
    fn ignores_ping_events() {
        let json = r#"{"type":"ping"}"#;
        assert!(parse_anthropic_chunk(json).is_empty());
    }

    #[test]
    fn ignores_malformed_json() {
        assert!(parse_anthropic_chunk("garbage").is_empty());
    }

    #[test]
    fn cache_control_marks_system_and_last_tool() {
        use crate::types::context::{ModelSpec, ProviderApi};
        let model = ModelSpec {
            id: "claude".into(),
            api: ProviderApi::Anthropic,
            max_tokens: 1024,
        };
        let tools = vec![ToolDefinition {
            name: "t".into(),
            label: "T".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
            risk: crate::types::tool::RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: crate::types::tool::ToolCallCtx| {
                Box::pin(async { Ok(crate::types::tool::ToolResult::text("")) })
            }),
        }];
        let body = build_request_body(&model, &[], "sys", &tools);
        // system is a content-block array with an ephemeral cache breakpoint.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[0]["text"], "sys");
        // the (last) tool carries a cache breakpoint.
        let tools_arr = body["tools"].as_array().expect("tools is array");
        assert_eq!(tools_arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_marks_final_message_block() {
        // The third breakpoint rides on the LAST content block of the
        // LAST message (after same-role folding), so the whole prior
        // conversation is cache-written this request and read next one.
        use crate::types::context::{ModelSpec, ProviderApi};
        let model = ModelSpec {
            id: "claude".into(),
            api: ProviderApi::Anthropic,
            max_tokens: 1024,
        };
        let messages = vec![
            AgentMessage::user("first"),
            AgentMessage::assistant_text("second"),
            AgentMessage::user("third"),
        ];
        let body = build_request_body(&model, &messages, "", &[]);
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 3);
        let last = msgs.last().unwrap();
        let blocks = last["content"].as_array().expect("content array");
        let block = blocks.last().unwrap();
        assert_eq!(block["cache_control"]["type"], "ephemeral");
        // No breakpoint on non-final messages (3 total with tools/system —
        // the earlier two live outside the messages array).
        for m in &msgs[..msgs.len() - 1] {
            let s = serde_json::to_string(m).unwrap();
            assert!(
                !s.contains("cache_control"),
                "non-final message marked: {s}"
            );
        }
    }

    // ── Role-alternation folding (Anthropic 400s on adjacent same roles) ──

    use crate::types::message::{
        AssistantMessage, FunctionCall, StopReason, ToolCall, ToolResultMessage, UserMessage,
    };

    fn request_body(messages: &[AgentMessage]) -> serde_json::Value {
        use crate::types::context::{ModelSpec, ProviderApi};
        let model = ModelSpec {
            id: "claude".into(),
            api: ProviderApi::Anthropic,
            max_tokens: 1024,
        };
        build_request_body(&model, messages, "", &[])
    }

    fn roles(body: &serde_json::Value) -> Vec<&str> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect()
    }

    fn user_msg(t: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(t)],
            timestamp: 0,
        })
    }

    fn assistant_tool_calls(ids: &[&str]) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolCall {
                    tool_call: ToolCall {
                        id: (*id).into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    },
                })
                .collect(),
            model: "claude".into(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        })
    }

    fn tool_result(id: &str, out: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text(out)],
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn folds_parallel_tool_results_into_one_user_message() {
        // One assistant turn with two tool calls → two ToolResult messages,
        // both `role: "user"` after conversion. Unfolded this is an instant
        // HTTP 400 ("roles must alternate").
        let messages = vec![
            user_msg("list files"),
            assistant_tool_calls(&["t1", "t2"]),
            tool_result("t1", "out1"),
            tool_result("t2", "out2"),
        ];
        let body = request_body(&messages);
        assert_eq!(roles(&body), vec!["user", "assistant", "user"]);
        let blocks = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "both tool_results in one user message");
        assert_eq!(blocks[0]["tool_use_id"], "t1");
        assert_eq!(blocks[1]["tool_use_id"], "t2");
    }

    #[test]
    fn folds_compaction_notice_ahead_of_kept_user_message() {
        // compact_by_count prepends a user notice; if the first kept
        // message is also user, the roles would repeat.
        let messages = vec![
            user_msg("[compacted 3 earlier messages]"),
            user_msg("real question"),
        ];
        let body = request_body(&messages);
        assert_eq!(roles(&body), vec!["user"]);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "[compacted 3 earlier messages]");
        assert_eq!(blocks[1]["text"], "real question");
    }

    #[test]
    fn folds_consecutive_assistant_messages() {
        let messages = vec![
            user_msg("q"),
            assistant_tool_calls(&["t1"]),
            tool_result("t1", "out1"),
            assistant_tool_calls(&["t2"]),
            tool_result("t2", "out2"),
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::text("done")],
                model: "claude".into(),
                stop_reason: StopReason::EndTurn,
                usage: None,
                timestamp: 0,
                stream_indices: Vec::new(),
            }),
        ];
        let body = request_body(&messages);
        assert_eq!(
            roles(&body),
            vec![
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "assistant"
            ]
        );
    }

    #[test]
    fn single_tool_round_trip_is_unchanged() {
        // The common case must not over-merge: one user, one assistant
        // tool_use, one tool_result → three alternating messages.
        let messages = vec![
            user_msg("q"),
            assistant_tool_calls(&["t1"]),
            tool_result("t1", "out1"),
        ];
        let body = request_body(&messages);
        assert_eq!(roles(&body), vec!["user", "assistant", "user"]);
        assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 1);
    }

    // ── Construction & Debug ───────────────────────────────────────

    #[test]
    fn with_client_keeps_the_base_url() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let p = AnthropicProvider::with_client("http://127.0.0.1:1234/v1", "sk-test", client);
        let out = format!("{p:?}");
        assert!(out.contains("AnthropicProvider"), "{out}");
        assert!(out.contains("http://127.0.0.1:1234/v1"), "{out}");
    }

    #[test]
    fn debug_format_never_leaks_the_api_key() {
        let p = AnthropicProvider::new("sk-super-secret");
        let out = format!("{p:?}");
        assert!(
            !out.contains("sk-super-secret"),
            "the api key must not reach the logs: {out}"
        );
    }

    #[test]
    fn with_base_url_overrides_the_default_endpoint() {
        let p = AnthropicProvider::new("k").with_base_url("http://127.0.0.1:9999/v1");
        let out = format!("{p:?}");
        assert!(out.contains("http://127.0.0.1:9999/v1"), "{out}");
        assert!(!out.contains("api.anthropic.com"), "{out}");
    }

    #[test]
    fn from_config_uses_the_configured_base_url() {
        use crate::test_util::fake_env;
        let cfg = crate::providers::ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.anthropic.com/v1"),
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "claude-sonnet"),
            ("CONGA_LLM_API", "anthropic"),
        ]))
        .unwrap();
        let p = AnthropicProvider::from_config(&cfg);
        let out = format!("{p:?}");
        assert!(out.contains("https://api.anthropic.com/v1"), "{out}");
        assert!(!out.contains("sk-test"), "{out}");
    }

    // ── StreamFn::stream ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_posts_an_anthropic_request_and_parses_the_sse_response() {
        use futures_util::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 1024];
            // Read the head (up to the blank line that ends it).
            let head_end = loop {
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&tmp[..n]);
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let content_len: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            // Read the body.
            while buf.len() - head_end < content_len {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.write_all(
                b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            )
            .await
            .unwrap();
            sock.write_all(
                b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
        });

        let provider = AnthropicProvider::with_client(
            format!("http://{addr}/v1"),
            "sk-test",
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        let model = ModelSpec {
            id: "claude".into(),
            api: crate::types::context::ProviderApi::Anthropic,
            max_tokens: 64,
        };
        let mut stream = provider.stream(&model, &[AgentMessage::user("hi")], "sys", &[], None);
        let mut chunks = Vec::new();
        while let Some(c) = stream.next().await {
            chunks.push(c);
        }
        server.await.unwrap();

        // The request the provider actually put on the wire.
        let req = rx.await.unwrap();
        assert!(req.starts_with("POST /v1/messages"), "{req}");
        assert!(
            req.to_lowercase().contains("x-api-key: sk-test"),
            "missing api key header: {req}"
        );
        assert!(
            req.to_lowercase().contains("anthropic-version: 2023-06-01"),
            "missing version header: {req}"
        );
        let body_start = req.find("\r\n\r\n").unwrap() + 4;
        let body: serde_json::Value = serde_json::from_str(&req[body_start..]).unwrap();
        assert_eq!(body["model"], "claude");
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["max_tokens"], json!(64));
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");

        // The response frames map to chunks.
        assert_eq!(chunks[0], StreamChunk::TextDelta("Hi".into()));
        assert_eq!(chunks[1], StreamChunk::Stop(StopReason::EndTurn));
        assert_eq!(
            chunks[2],
            StreamChunk::Usage {
                input: 0,
                output: 3,
                cache_read: 0,
                cache_write: 0
            }
        );
        assert_eq!(chunks.last(), Some(&StreamChunk::Done));
    }

    // ── convert_message ────────────────────────────────────────────

    #[test]
    fn convert_message_drops_a_user_message_without_text() {
        let empty = AgentMessage::User(UserMessage {
            content: vec![],
            timestamp: 0,
        });
        assert!(convert_message(&empty).is_none());
    }

    #[test]
    fn convert_message_keeps_every_text_block_of_a_user_message() {
        let msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text("a"), ContentBlock::text("b")],
            timestamp: 0,
        });
        let v = convert_message(&msg).expect("user converts");
        assert_eq!(v["role"], "user");
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "a");
        assert_eq!(blocks[1]["text"], "b");
    }

    #[test]
    fn convert_message_drops_an_assistant_message_without_text_or_tool_calls() {
        let empty = AgentMessage::Assistant(AssistantMessage {
            content: vec![],
            model: "claude".into(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        });
        assert!(convert_message(&empty).is_none());
    }

    #[test]
    fn convert_message_drops_an_assistant_message_with_thinking_only() {
        // Thinking blocks are not part of the Anthropic wire format here.
        let msg = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
            }],
            model: "claude".into(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        });
        assert!(convert_message(&msg).is_none());
    }

    #[test]
    fn convert_message_maps_an_assistant_tool_call_to_tool_use() {
        let msg = AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::text("thinking out loud"),
                ContentBlock::ToolCall {
                    tool_call: ToolCall {
                        id: "toolu_1".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"cmd":"ls"}"#.into(),
                        },
                    },
                },
            ],
            model: "claude".into(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        });
        let v = convert_message(&msg).expect("assistant converts");
        assert_eq!(v["role"], "assistant");
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "toolu_1");
        assert_eq!(blocks[1]["name"], "bash");
        assert_eq!(blocks[1]["input"]["cmd"], "ls");
    }

    #[test]
    fn convert_message_falls_back_to_an_empty_input_for_broken_arguments() {
        // A tool call with unparseable arguments must still convert: the
        // input degrades to `{}` instead of dropping the whole call.
        let msg = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "toolu_1".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: "{not json".into(),
                    },
                },
            }],
            model: "claude".into(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        });
        let v = convert_message(&msg).expect("assistant converts");
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"], json!({}));
    }

    #[test]
    fn convert_message_maps_a_tool_result_to_a_user_tool_result_block() {
        let msg = tool_result("t1", "out1");
        let v = convert_message(&msg).expect("tool result converts");
        assert_eq!(v["role"], "user");
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "t1");
        assert_eq!(blocks[0]["content"], "out1");
        assert_eq!(blocks[0]["is_error"], json!(false));
    }

    #[test]
    fn convert_message_marks_an_errored_tool_result() {
        let mut msg = tool_result("t1", "boom");
        if let AgentMessage::ToolResult(tr) = &mut msg {
            tr.is_error = true;
        }
        let v = convert_message(&msg).expect("tool result converts");
        assert_eq!(v["content"][0]["is_error"], json!(true));
    }

    #[test]
    fn convert_message_drops_a_custom_message() {
        let custom = AgentMessage::Custom(crate::types::message::CustomMessage {
            custom_type: "x".into(),
            content: json!({}),
            timestamp: 0,
        });
        assert!(convert_message(&custom).is_none());
    }

    // ── fold_same_role ─────────────────────────────────────────────

    #[test]
    fn fold_same_role_on_an_empty_list() {
        assert!(fold_same_role(Vec::new()).is_empty());
    }

    #[test]
    fn fold_same_role_keeps_a_single_message() {
        let folded = fold_same_role(vec![json!({"role": "user", "content": []})]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0]["role"], "user");
    }

    #[test]
    fn fold_same_role_merges_two_user_messages() {
        let folded = fold_same_role(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "b"}]}),
        ]);
        assert_eq!(folded.len(), 1);
        let blocks = folded[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "a");
        assert_eq!(blocks[1]["text"], "b");
    }

    #[test]
    fn fold_same_role_merges_three_messages_of_the_same_role() {
        let folded = fold_same_role(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "c"}]}),
        ]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0]["content"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn fold_same_role_keeps_alternating_roles_apart() {
        let folded = fold_same_role(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ]);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0]["role"], "user");
        assert_eq!(folded[1]["role"], "assistant");
    }

    #[test]
    fn fold_same_role_merges_a_tool_result_into_the_preceding_user_message() {
        // Both are `role: "user"` on the wire: the tool_result block must
        // join the user message, not become a second user turn.
        let folded = fold_same_role(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "q"}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]}),
        ]);
        assert_eq!(folded.len(), 1);
        let blocks = folded[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "tool_result");
    }

    #[test]
    fn fold_same_role_does_not_merge_a_message_without_a_content_array() {
        // Nothing to concatenate: the messages must stay separate.
        let folded = fold_same_role(vec![json!({"role": "user"}), json!({"role": "user"})]);
        assert_eq!(folded.len(), 2);
    }

    // ── tool_to_anthropic ──────────────────────────────────────────

    #[test]
    fn tool_to_anthropic_maps_the_definition() {
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
        let v = tool_to_anthropic(&tool);
        assert_eq!(v["name"], "read");
        assert_eq!(v["description"], "read a file");
        assert_eq!(v["input_schema"]["type"], "object");
        assert_eq!(v["input_schema"]["properties"]["path"]["type"], "string");
        // The Anthropic wire format uses `input_schema`, not `parameters`.
        assert!(v.get("parameters").is_none(), "{v}");
        // Internal fields must not leak.
        assert!(v.get("label").is_none());
        assert!(v.get("risk").is_none());
    }

    #[test]
    fn tool_to_anthropic_with_empty_parameters() {
        let tool = ToolDefinition {
            name: "ping".into(),
            label: "Ping".into(),
            description: String::new(),
            parameters: json!({}),
            risk: crate::types::tool::RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: crate::types::tool::ToolCallCtx| {
                Box::pin(async { Ok(crate::types::tool::ToolResult::text("")) })
            }),
        };
        let v = tool_to_anthropic(&tool);
        assert_eq!(v["name"], "ping");
        assert_eq!(v["description"], "");
        assert_eq!(v["input_schema"], json!({}));
    }
}
