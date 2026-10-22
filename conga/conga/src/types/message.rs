//! `AgentMessage` — the internal unified message model.
//!
//! All messages carry a `timestamp` (ms since epoch via [`crate::now`]).
//! `CustomMessage` is plugin-private and filtered out at the LLM boundary.

use serde::{Deserialize, Serialize};

/// The single internal message enum. Converted to provider protocol only at
/// the LLM boundary (`convert_to_llm`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    /// Plugin-private message. Never sent to the LLM (filtered by
    /// `convert_to_llm`). The `custom_type` namespace is owned by the plugin.
    Custom(CustomMessage),
}
impl AgentMessage {
    /// User message carrying a single text block. Convenience constructor
    /// for hosts and tests that don't build block lists by hand.
    pub fn user(text: impl Into<String>) -> Self {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: crate::now(),
        })
    }

    /// Assistant message carrying a single text block, no usage, and an
    /// empty model id (fill it in when the real model is known). Mirrors the
    /// defaults of [`AssistantMessage::new`].
    pub fn assistant_text(text: impl Into<String>) -> Self {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(text)],
            model: String::new(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: crate::now(),
            stream_indices: Vec::new(),
        })
    }
}

/// A user-authored message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

/// An assistant (model) message. `content` may hold text, thinking, and tool
/// calls simultaneously.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: ModelId,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub timestamp: u64,
    /// Streaming-only routing table: `tool_calls[].index` (OpenAI-compat) of
    /// each ToolCall block, in creation order. Never serialized — persisted
    /// messages are already fully assembled.
    #[serde(skip)]
    pub stream_indices: Vec<u32>,
}

impl AssistantMessage {
    /// Empty assistant message tagged with the model in use, for streaming
    /// accumulation in `stream_assistant_response`.
    pub fn new(model: &ModelId) -> Self {
        Self {
            content: Vec::new(),
            model: model.clone(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: crate::now(),
            stream_indices: Vec::new(),
        }
    }

    /// Append a text delta to the last Text block (or start a new one).
    pub fn append_text(&mut self, delta: &str) {
        if let Some(ContentBlock::Text { text }) = self.content.last_mut() {
            text.push_str(delta);
        } else {
            self.content.push(ContentBlock::text(delta));
        }
    }

    /// Append a thinking delta to the last Thinking block (or start a new one).
    pub fn append_thinking(&mut self, delta: &str) {
        if let Some(ContentBlock::Thinking { thinking }) = self.content.last_mut() {
            thinking.push_str(delta);
        } else {
            self.content.push(ContentBlock::Thinking {
                thinking: delta.to_string(),
            });
        }
    }

    /// Accumulate a tool-call delta: create the call on first sight, then
    /// append argument fragments as they stream in.
    ///
    /// The delta is routed to an existing ToolCall block by its ordinal
    /// among ToolCall blocks, resolved by the first key that applies:
    ///
    /// - `index` (OpenAI-compat `tool_calls[].index`) — parallel calls
    ///   interleave freely; first sight of an index opens a new call.
    /// - `id` (Anthropic `content_block_start`) — match the call opened
    ///   with that id (last match wins, as ids are unique per stream).
    /// - neither (OpenAI-compat servers omitting `index`, Anthropic
    ///   `input_json_delta`) — the most recent call, which is correct for
    ///   sequential streaming and for Anthropic's non-interleaved blocks.
    pub fn append_tool_call(
        &mut self,
        index: Option<u32>,
        id: String,
        name: Option<String>,
        args_delta: String,
    ) {
        let nth = match index {
            Some(i) => self.stream_indices.iter().position(|&s| s == i),
            None if !id.is_empty() => {
                let mut nth = 0usize;
                let mut found = None;
                for block in &self.content {
                    if let ContentBlock::ToolCall { tool_call } = block {
                        if tool_call.id == id {
                            found = Some(nth);
                        }
                        nth += 1;
                    }
                }
                found
            }
            None => self
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                .count()
                .checked_sub(1),
        };

        let mut ordinal = 0usize;
        let target = self.content.iter_mut().find_map(|b| match b {
            ContentBlock::ToolCall { tool_call } => {
                let hit = Some(ordinal) == nth;
                ordinal += 1;
                hit.then_some(tool_call)
            }
            _ => None,
        });

        match target {
            Some(tc) => {
                if let Some(name) = name {
                    tc.function.name = name;
                }
                tc.function.arguments.push_str(&args_delta);
            }
            None => {
                if let Some(i) = index {
                    self.stream_indices.push(i);
                }
                self.content.push(ContentBlock::ToolCall {
                    tool_call: ToolCall {
                        id,
                        function: FunctionCall {
                            name: name.unwrap_or_default(),
                            arguments: args_delta,
                        },
                    },
                });
            }
        }
    }
}

/// A tool-result message, paired with the `tool_call_id` it answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub timestamp: u64,
}

/// A plugin-private message. `custom_type` is the plugin's namespace
/// (e.g. `"todo.list"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: serde_json::Value,
    pub timestamp: u64,
}

/// One block of message content. An assistant turn may hold several of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        tool_call: ToolCall,
    },
    /// Model reasoning content (extended thinking).
    Thinking {
        thinking: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON arguments string from the model (validated at execution time).
    pub arguments: String,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished naturally.
    EndTurn,
    /// Model emitted tool calls; loop should execute them and continue.
    ToolUse,
    /// Output hit the token cap.
    MaxTokens,
    /// Provider/stream returned an error.
    Error(String),
    /// Aborted via the abort signal.
    Aborted,
}

/// A model identifier (provider + model id). Lightweight string wrapper so
/// providers can format it as they see fit.
pub type ModelId = String;

/// Token usage reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache. `None` when the
    /// provider didn't report cache stats (old logs stay parseable).
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into the provider's cache this call.
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_calls(msg: &AssistantMessage) -> Vec<&ToolCall> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { tool_call } => Some(tool_call),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn index_keyed_interleaved_deltas_route_to_their_own_call() {
        // OpenAI-compat streams parallel tool calls interleaved, keyed by
        // `index`: A-open, B-open, A-cont, B-cont. Each fragment must land on
        // its own call; appending continuations to the most recent call (the
        // pre-fix behavior) corrupts both argument strings.
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(
            Some(0),
            "t0".into(),
            Some("get_weather".into()),
            "{\"city\":".into(),
        );
        m.append_tool_call(
            Some(1),
            "t1".into(),
            Some("get_time".into()),
            "{\"tz\":".into(),
        );
        m.append_tool_call(Some(0), String::new(), None, "\"Cairo\"}".into());
        m.append_tool_call(Some(1), String::new(), None, "\"UTC\"}".into());

        let calls = tool_calls(&m);
        assert_eq!(calls.len(), 2, "two calls, not one merged blob");
        assert_eq!(calls[0].id, "t0");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, "{\"city\":\"Cairo\"}");
        assert_eq!(calls[1].id, "t1");
        assert_eq!(calls[1].function.name, "get_time");
        assert_eq!(calls[1].function.arguments, "{\"tz\":\"UTC\"}");
    }

    #[test]
    fn index_continuation_without_prior_open_starts_new_call() {
        // Defensive: a continuation index never seen before starts a call
        // rather than corrupting a neighbor (real streams open with
        // id+name first, but the accumulator must not depend on that).
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(Some(3), String::new(), None, "{}".into());
        let calls = tool_calls(&m);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{}");
    }

    #[test]
    fn empty_id_empty_index_appends_to_last_call() {
        // Sequential streaming (the common case) and Anthropic's
        // input_json_delta: no index, no id - append to the most recent
        // call. Unchanged pre-existing behavior, pinned as a regression.
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(None, "t1".into(), Some("echo".into()), "{\"x\":".into());
        m.append_tool_call(None, String::new(), None, "1}".into());
        let calls = tool_calls(&m);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{\"x\":1}");
    }

    #[test]
    fn id_keyed_open_then_idless_continuation_appends_to_that_call() {
        // Anthropic shape: content_block_start carries the real id+name;
        // subsequent input_json_delta carries neither id nor index.
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(None, "toolu_1".into(), Some("read".into()), String::new());
        m.append_tool_call(None, String::new(), None, "{\"path\":".into());
        m.append_tool_call(None, String::new(), None, "\"a\"}".into());
        let calls = tool_calls(&m);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn text_between_tool_deltas_does_not_break_index_routing() {
        // Interleaved text/thinking blocks shift raw content positions;
        // index routing must count ToolCall blocks, not content indexes.
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(Some(0), "t0".into(), Some("a".into()), "{\"x\":".into());
        m.append_text("working on it");
        m.append_tool_call(Some(1), "t1".into(), Some("b".into()), "{\"y\":".into());
        m.append_tool_call(Some(0), String::new(), None, "1}".into());
        let calls = tool_calls(&m);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.arguments, "{\"x\":1}");
        assert_eq!(calls[1].function.arguments, "{\"y\":");
    }

    #[test]
    fn stream_routing_state_is_not_serialized() {
        // Disk-format invariance: the accumulation-side index table must
        // never leak into the persisted SessionEvent::Assistant payload.
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_tool_call(Some(0), "t0".into(), Some("f".into()), "{}".into());
        let json = serde_json::to_value(&m).unwrap();
        assert!(
            json.get("stream_indices").is_none(),
            "routing state leaked into serialized form: {json}"
        );
        let back: AssistantMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back.content, m.content);
    }

    #[test]
    fn usage_old_log_json_parses_with_absent_cache_fields() {
        // Old events.jsonl lines carry only input/output; the cache fields
        // must default to None (absent = not reported), never fail.
        let u: Usage = serde_json::from_str(r#"{"input_tokens":5,"output_tokens":3}"#).unwrap();
        assert_eq!(u.input_tokens, 5);
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cache_read_tokens, None);
        assert_eq!(u.cache_write_tokens, None);
    }

    #[test]
    fn usage_cache_fields_round_trip() {
        let u = Usage {
            input_tokens: 5,
            output_tokens: 3,
            cache_read_tokens: Some(7),
            cache_write_tokens: Some(11),
        };
        let json = serde_json::to_value(u).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "input_tokens": 5,
                "output_tokens": 3,
                "cache_read_tokens": 7,
                "cache_write_tokens": 11,
            })
        );
        let back: Usage = serde_json::from_value(json).unwrap();
        assert_eq!(back, u);
    }

    // ── AgentMessage::user ────────────────────────────────────────

    #[test]
    fn user_message_wraps_the_text_in_one_block() {
        let m = AgentMessage::user("hello");
        match &m {
            AgentMessage::User(u) => {
                assert_eq!(u.content, vec![ContentBlock::text("hello")]);
                assert!(u.timestamp > 0, "timestamp must be set");
            }
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn user_message_keeps_an_empty_text_block() {
        // An empty prompt is still a prompt: the block exists, it is empty.
        let m = AgentMessage::user("");
        match &m {
            AgentMessage::User(u) => assert_eq!(u.content, vec![ContentBlock::text("")]),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn user_message_preserves_newlines_and_unicode() {
        let m = AgentMessage::user("第一行\n第二行 ✅");
        match &m {
            AgentMessage::User(u) => {
                assert_eq!(u.content, vec![ContentBlock::text("第一行\n第二行 ✅")]);
            }
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn user_message_serializes_with_the_user_role_tag() {
        let json = serde_json::to_value(AgentMessage::user("hi")).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hi");
    }

    // ── AgentMessage::assistant_text ──────────────────────────────

    #[test]
    fn assistant_text_message_carries_one_text_block_and_defaults() {
        let m = AgentMessage::assistant_text("answer");
        match &m {
            AgentMessage::Assistant(a) => {
                assert_eq!(a.content, vec![ContentBlock::text("answer")]);
                assert_eq!(a.model, "", "the model id is filled in later");
                assert_eq!(a.stop_reason, StopReason::EndTurn);
                assert_eq!(a.usage, None);
                assert!(a.stream_indices.is_empty());
                assert!(a.timestamp > 0);
            }
            other => panic!("expected an assistant message, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_message_with_empty_text() {
        let m = AgentMessage::assistant_text("");
        match &m {
            AgentMessage::Assistant(a) => assert_eq!(a.content, vec![ContentBlock::text("")]),
            other => panic!("expected an assistant message, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_message_serializes_with_the_assistant_role_tag() {
        let json = serde_json::to_value(AgentMessage::assistant_text("hi")).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"][0]["text"], "hi");
    }

    // ── AssistantMessage::new / append_text / append_thinking ─────

    #[test]
    fn assistant_message_new_is_empty_but_tagged() {
        let m = AssistantMessage::new(&"glm".to_string());
        assert!(m.content.is_empty());
        assert_eq!(m.model, "glm");
        assert_eq!(m.stop_reason, StopReason::EndTurn);
        assert_eq!(m.usage, None);
        assert!(m.stream_indices.is_empty());
    }

    #[test]
    fn append_text_appends_to_the_last_text_block() {
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_text("he");
        m.append_text("llo");
        assert_eq!(m.content, vec![ContentBlock::text("hello")]);
    }

    #[test]
    fn append_text_starts_a_new_block_after_a_thinking_block() {
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_thinking("thinking");
        m.append_text("answer");
        assert_eq!(m.content.len(), 2);
        assert_eq!(
            m.content[0],
            ContentBlock::Thinking {
                thinking: "thinking".to_string()
            }
        );
        assert_eq!(m.content[1], ContentBlock::text("answer"));
    }

    #[test]
    fn append_thinking_creates_then_appends_to_the_same_block() {
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_thinking("par");
        m.append_thinking("tial");
        assert_eq!(
            m.content,
            vec![ContentBlock::Thinking {
                thinking: "partial".to_string()
            }]
        );
    }

    #[test]
    fn append_thinking_after_text_starts_a_new_block() {
        // The delta only ever joins the LAST block when that block is a
        // thinking block; a text block in between starts a fresh thinking
        // block (content order is preserved).
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_text("said");
        m.append_thinking("thought");
        assert_eq!(m.content.len(), 2);
        assert_eq!(m.content[0], ContentBlock::text("said"));
        assert_eq!(
            m.content[1],
            ContentBlock::Thinking {
                thinking: "thought".to_string()
            }
        );
    }

    #[test]
    fn append_thinking_with_an_empty_delta_creates_an_empty_block() {
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_thinking("");
        assert_eq!(
            m.content,
            vec![ContentBlock::Thinking {
                thinking: String::new()
            }]
        );
    }

    #[test]
    fn append_thinking_keeps_unicode_intact() {
        let mut m = AssistantMessage::new(&"m".to_string());
        m.append_thinking("思考");
        m.append_thinking("中");
        assert_eq!(
            m.content,
            vec![ContentBlock::Thinking {
                thinking: "思考中".to_string()
            }]
        );
    }

    // ── ContentBlock::text ────────────────────────────────────────

    #[test]
    fn content_block_text_builds_a_text_block() {
        assert_eq!(
            ContentBlock::text("x"),
            ContentBlock::Text {
                text: "x".to_string()
            }
        );
    }

    #[test]
    fn content_block_text_accepts_an_empty_string() {
        assert_eq!(
            ContentBlock::text(""),
            ContentBlock::Text {
                text: String::new()
            }
        );
    }

    #[test]
    fn content_block_text_serializes_with_the_text_type_tag() {
        let json = serde_json::to_value(ContentBlock::text("x")).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "x");
    }
}
