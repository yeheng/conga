//! EventPrinter: 消费 AgentEvent，渲染到注入的 writer（可测/可复用）。
use std::io::Write;

use conga::{AgentEvent, ContentDelta};

pub struct EventPrinter<W: Write> {
    out: W,
}

impl<W: Write> EventPrinter<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    pub fn on_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::MessageUpdate { delta } => match delta {
                ContentDelta::TextDelta(t) => {
                    let _ = self.out.write_all(t.as_bytes());
                }
                ContentDelta::ThinkingDelta(_) => {}
                ContentDelta::ToolCallDelta { .. } => {}
            },
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let _ = writeln!(self.out, "\n-> {tool_name} {}", args);
            }
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => {
                let first = result
                    .content
                    .iter()
                    .find_map(|b| match b {
                        conga::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let head = first.lines().next().unwrap_or("");
                let tag = if *is_error { "ERR" } else { "ok" };
                let _ = writeln!(self.out, "   [{tag}] {head}");
            }
            AgentEvent::AfterProviderResponse { response, .. } => {
                if let Some(u) = &response.usage {
                    // Cache segment only when the provider reported cache
                    // tokens (read↔write); most OpenAI-compat providers
                    // report none — keep their line unchanged.
                    let cr = u.cache_read_tokens.unwrap_or(0);
                    let cw = u.cache_write_tokens.unwrap_or(0);
                    let cache = if cr > 0 || cw > 0 {
                        format!(", cache: {cr}↔{cw}")
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        self.out,
                        "\n[in: {}, out: {}{}]",
                        u.input_tokens, u.output_tokens, cache
                    );
                }
            }
            AgentEvent::TurnEnd { .. } => {
                let _ = writeln!(self.out);
            }
            AgentEvent::Error { message } => {
                let _ = writeln!(self.out, "\n[error] {message}");
            }
            _ => {}
        }
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::AssistantMessage;

    #[test]
    fn streams_text_delta() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("Hi".into()),
        });
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta(" there".into()),
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "Hi there");
    }

    #[test]
    fn error_event_renders() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::Error {
            message: "boom".into(),
        });
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[error] boom"));
    }

    #[test]
    fn flush_after_every_event() {
        // Vec<u8> flush is a no-op, but the call path must run without panic
        // and leave the buffer intact (pipes rely on the flush to drain).
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("x".into()),
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "x");
    }

    #[test]
    fn prints_usage_after_response() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        let model = "m".to_string();
        let mut msg = AssistantMessage::new(&model);
        msg.usage = Some(conga::types::message::Usage {
            input_tokens: 42,
            output_tokens: 7,
            cache_read_tokens: Some(100),
            cache_write_tokens: Some(50),
        });
        let _ = msg.stop_reason;
        p.on_event(&AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response: msg,
        });
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("[in: 42, out: 7, cache: 100↔50]"),
            "cache-reported usage line: {s:?}"
        );

        // No cache breakdown (None/0) -> the classic two-field line.
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        let mut msg = AssistantMessage::new(&model);
        msg.usage = Some(conga::types::message::Usage {
            input_tokens: 42,
            output_tokens: 7,
            cache_read_tokens: None,
            cache_write_tokens: None,
        });
        let _ = msg.stop_reason;
        p.on_event(&AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response: msg,
        });
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("[in: 42, out: 7]") && !s.contains("cache"),
            "cache must be omitted when unreported: {s:?}"
        );
    }

    #[test]
    fn tool_execution_start_prints_the_tool_and_its_args() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
        });
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\n-> bash "), "{s:?}");
        assert!(s.contains("ls"), "{s:?}");
        assert!(s.ends_with('\n'), "{s:?}");
    }

    #[test]
    fn tool_execution_end_prints_the_first_line_with_a_status_tag() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        let result = conga::ToolResultMessage {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            content: vec![conga::ContentBlock::text("first line\nsecond line")],
            is_error: false,
            timestamp: 0,
        };
        p.on_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc1".into(),
            result,
            is_error: false,
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "   [ok] first line\n");

        // An errored call is tagged ERR; an empty result still prints a line.
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        let result = conga::ToolResultMessage {
            tool_call_id: "tc2".into(),
            tool_name: "bash".into(),
            content: vec![],
            is_error: true,
            timestamp: 0,
        };
        p.on_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc2".into(),
            result,
            is_error: true,
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "   [ERR] \n");
    }

    #[test]
    fn thinking_deltas_and_usage_less_responses_print_nothing() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::ThinkingDelta("hmm".into()),
        });
        p.on_event(&AgentEvent::MessageUpdate {
            delta: ContentDelta::ToolCallDelta {
                id: "x".into(),
                name: None,
                args_delta: "{}".into(),
            },
        });
        // A provider response without usage must not print a usage line.
        p.on_event(&AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response: conga::AssistantMessage::new(&"m".to_string()),
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "");
    }

    #[test]
    fn turn_end_prints_a_single_newline() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&AgentEvent::TurnEnd {
            message: conga::AssistantMessage::new(&"m".to_string()),
            tool_results: vec![],
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "\n");
    }
}
