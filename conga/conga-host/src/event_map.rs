//! Map core [`AgentEvent`]s to the frontend's streaming chat protocol.
//!
//! The mapping lives in the host so every transport (gateway WebSocket,
//! Tauri IPC) emits one identical event schema.

use std::collections::HashMap;

use conga::{AgentEvent, ContentDelta};

use crate::wire::OutgoingEvent;

/// Convert an [`AgentEvent`] to the frontend's JSON protocol, looking up
/// tool names from `tool_names` (populated by [`ToolExecutionStart`]). For
/// `ToolExecutionEnd` without a preceding start (denied/timed-out/cancelled
/// tool calls) the name falls back to the one carried by the result message.
pub fn event_to_ws(
    event: &AgentEvent,
    tool_names: &mut HashMap<String, String>,
) -> Option<OutgoingEvent> {
    match event {
        AgentEvent::MessageUpdate { delta } => match delta {
            ContentDelta::TextDelta(t) => Some(OutgoingEvent::content(t.clone())),
            ContentDelta::ThinkingDelta(t) => Some(OutgoingEvent::thinking(t.clone())),
            ContentDelta::ToolCallDelta { .. } => {
                // Accumulated server-side; sent as tool_start at execution.
                None
            }
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
            ..
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            Some(OutgoingEvent::tool_start(
                tool_name.clone(),
                args_str,
                tool_call_id.clone(),
            ))
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            let name = tool_names
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| result.tool_name.clone());
            let summary = result
                .content
                .iter()
                .find_map(|b| match b {
                    conga::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(OutgoingEvent::tool_end(name, summary, tool_call_id.clone()))
        }
        AgentEvent::Error { message } => Some(OutgoingEvent::error(message.clone())),
        _ => None,
    }
}

/// Convert a [`SubagentEvent`] to a JSON value for the frontend's
/// `subagent_*` protocol. Returns `None` for events that have no wire
/// representation (the internal `Usage` accounting event). Transports
/// serialize the value themselves (WS: text frame; Tauri: event payload).
pub fn subagent_event_to_ws(
    event: &crate::subagent_types::SubagentEvent,
) -> Option<serde_json::Value> {
    use crate::subagent_types::SubagentEvent;
    let json = match event {
        SubagentEvent::AllStarted { count } => serde_json::json!({
            "type": "subagent_all_started", "count": count
        }),
        SubagentEvent::Synthesizing => serde_json::json!({
            "type": "subagent_synthesizing"
        }),
        SubagentEvent::Started { id, task, index } => serde_json::json!({
            "type": "subagent_started", "id": id, "task": task, "index": index
        }),
        SubagentEvent::Thinking { id, content } => serde_json::json!({
            "type": "subagent_thinking", "id": id, "content": content
        }),
        SubagentEvent::Content { id, content } => serde_json::json!({
            "type": "subagent_content", "id": id, "content": content
        }),
        SubagentEvent::ToolStart {
            id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "subagent_tool_start", "id": id, "name": name,
            "arguments": arguments
        }),
        SubagentEvent::ToolEnd { id, name, output } => serde_json::json!({
            "type": "subagent_tool_end", "id": id, "name": name,
            "output": output
        }),
        SubagentEvent::Completed {
            id,
            index,
            summary,
            tool_count,
        } => serde_json::json!({
            "type": "subagent_completed", "id": id, "index": index,
            "summary": summary, "tool_count": tool_count
        }),
        SubagentEvent::Error { id, index, error } => serde_json::json!({
            "type": "subagent_error", "id": id, "index": index, "error": error
        }),
        // Internal accounting only — folded into the session usage counters
        // by the WS forwarder; never serialized to the frontend.
        SubagentEvent::Usage { .. } => return None,
    };
    Some(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_types::SubagentEvent;
    use conga::{ContentBlock, ToolResultMessage};
    use serde_json::Value;

    #[test]
    fn usage_has_no_wire_representation() {
        assert!(
            subagent_event_to_ws(&SubagentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read: 3,
                cache_write: 4,
            })
            .is_none(),
            "Usage is internal accounting and must never reach the socket"
        );
    }

    #[test]
    fn tool_end_json_has_no_tool_id() {
        // The frontend generates its own tool ids and matches by name;
        // the server's tool_call_id is deliberately not serialized.
        let v = subagent_event_to_ws(&SubagentEvent::ToolEnd {
            id: "s1".into(),
            name: "bash".into(),
            output: Some("out".into()),
        })
        .unwrap();
        assert_eq!(v["type"], "subagent_tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["id"], "s1");
        assert!(v.get("tool_id").is_none(), "no dead tool_id on the wire");
    }

    /// A `ToolExecutionEnd` whose result carries the given tool name/text,
    /// mirroring what the core emits for denied/timed-out/cancelled calls.
    fn tool_end_event(tool_call_id: &str, tool_name: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call_id.into(),
            result: ToolResultMessage {
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                content: vec![ContentBlock::Text { text: text.into() }],
                is_error: true,
                timestamp: 0,
            },
            is_error: true,
        }
    }

    fn ws_json(event: &AgentEvent, tool_names: &mut HashMap<String, String>) -> Value {
        let ws = event_to_ws(event, tool_names).expect("event maps to an OutgoingEvent");
        serde_json::to_value(&ws).expect("OutgoingEvent serializes")
    }

    #[test]
    fn tool_end_uses_registered_tool_name_when_start_was_seen() {
        let mut tool_names = HashMap::new();
        tool_names.insert("tc1".into(), "bash".into());
        let v = ws_json(&tool_end_event("tc1", "bash", "ok"), &mut tool_names);
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["output"], "ok");
        assert_eq!(v["tool_call_id"], "tc1");
    }

    #[test]
    fn tool_end_falls_back_to_result_tool_name() {
        // Denied/timed-out/cancelled calls have no preceding ToolExecutionStart,
        // so `tool_names` is empty - the name must come from the result message.
        let mut tool_names = HashMap::new();
        let v = ws_json(
            &tool_end_event("tc1", "bash", "approval denied by user"),
            &mut tool_names,
        );
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["output"], "approval denied by user");
        assert_eq!(v["tool_call_id"], "tc1");
    }

    #[test]
    fn tool_start_serializes_tool_call_id() {
        let mut tool_names = HashMap::new();
        let event = AgentEvent::ToolExecutionStart {
            tool_call_id: "tc9".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
        };
        let v = ws_json(&event, &mut tool_names);
        assert_eq!(v["type"], "tool_start");
        assert_eq!(v["tool_call_id"], "tc9");
        assert_eq!(v["name"], "bash");
    }

    #[test]
    fn text_delta_maps_to_content_event() {
        let mut tool_names = HashMap::new();
        let event = AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("hello".into()),
        };
        let v = ws_json(&event, &mut tool_names);
        assert_eq!(v["type"], "content");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn thinking_delta_maps_to_thinking_event() {
        let mut tool_names = HashMap::new();
        let event = AgentEvent::MessageUpdate {
            delta: ContentDelta::ThinkingDelta("pondering".into()),
        };
        let v = ws_json(&event, &mut tool_names);
        assert_eq!(v["type"], "thinking");
        assert_eq!(v["content"], "pondering");
    }

    #[test]
    fn error_event_maps_to_the_error_event() {
        let mut tool_names = HashMap::new();
        let v = ws_json(
            &AgentEvent::Error {
                message: "boom".into(),
            },
            &mut tool_names,
        );
        assert_eq!(v["type"], "error");
        assert_eq!(v["content"], "boom");
        assert_eq!(v["message"], "boom");
    }

    #[test]
    fn tool_end_without_any_text_block_reports_an_empty_output() {
        let mut tool_names = HashMap::new();
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc1".into(),
            result: ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::Thinking {
                    thinking: "hmm".into(),
                }],
                is_error: false,
                timestamp: 0,
            },
            is_error: false,
        };
        let v = ws_json(&event, &mut tool_names);
        assert_eq!(v["type"], "tool_end");
        assert_eq!(v["output"], "");
    }

    #[test]
    fn subagent_events_map_to_their_wire_types() {
        let v = subagent_event_to_ws(&SubagentEvent::AllStarted { count: 3 }).unwrap();
        assert_eq!(v["type"], "subagent_all_started");
        assert_eq!(v["count"], 3);

        let v = subagent_event_to_ws(&SubagentEvent::Synthesizing).unwrap();
        assert_eq!(v["type"], "subagent_synthesizing");

        let v = subagent_event_to_ws(&SubagentEvent::Started {
            id: "s1".into(),
            task: "do it".into(),
            index: 0,
        })
        .unwrap();
        assert_eq!(v["type"], "subagent_started");
        assert_eq!(v["id"], "s1");
        assert_eq!(v["task"], "do it");
        assert_eq!(v["index"], 0);

        let v = subagent_event_to_ws(&SubagentEvent::Completed {
            id: "s1".into(),
            index: 1,
            summary: "done".into(),
            tool_count: 2,
        })
        .unwrap();
        assert_eq!(v["type"], "subagent_completed");
        assert_eq!(v["summary"], "done");
        assert_eq!(v["tool_count"], 2);

        let v = subagent_event_to_ws(&SubagentEvent::Error {
            id: "s1".into(),
            index: 0,
            error: "bad".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "subagent_error");
        assert_eq!(v["error"], "bad");
    }

    #[test]
    fn unhandled_events_map_to_none() {
        let mut tool_names = HashMap::new();
        let events = [
            AgentEvent::AgentStart,
            AgentEvent::AgentEnd,
            AgentEvent::TurnStart,
            AgentEvent::MessageStart,
            AgentEvent::MessageUpdate {
                delta: ContentDelta::ToolCallDelta {
                    id: "x".into(),
                    name: None,
                    args_delta: "{}".into(),
                },
            },
        ];
        for event in events {
            assert!(
                event_to_ws(&event, &mut tool_names).is_none(),
                "unexpected mapping for {event:?}"
            );
        }
    }
}
