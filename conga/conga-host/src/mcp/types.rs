//! Shared JSON-RPC wire types, MCP content mapping, and the tool wrapper
//! used identically by both the stdio and HTTP transports.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use conga::{ContentBlock, RiskLevel, ToolDefinition, ToolError, ToolResult};
use serde::Deserialize;

use super::McpError;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct McpTool {
    pub(super) name: String,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) description: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) annotations: serde_json::Value,
    #[serde(rename = "inputSchema")]
    pub(super) input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub(super) struct ToolsListResult {
    #[serde(default, rename = "tools")]
    pub(super) list: Vec<McpTool>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CallResult {
    #[serde(default)]
    pub(super) content: Vec<McpContent>,
    #[serde(default)]
    pub(super) is_error: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct McpContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
}

/// Map MCP content items to conga's `ContentBlock`s.
pub(super) fn content_to_blocks(items: &[McpContent]) -> Vec<ContentBlock> {
    let blocks: Vec<ContentBlock> = items
        .iter()
        .filter_map(|c| match c.kind.as_str() {
            "text" => c.text.clone().map(ContentBlock::text),
            // Vision is not on conga's wire: providers would silently drop
            // image blocks. Say so instead of constructing a block the model
            // never sees.
            "image" => Some(ContentBlock::text(format!(
                "[image content omitted: {}, {} bytes of base64]",
                c.mime_type.as_deref().unwrap_or("unknown mime"),
                c.data.as_deref().map(str::len).unwrap_or(0),
            ))),
            _ => Some(ContentBlock::text(format!("{c:?}"))),
        })
        .collect();
    if blocks.is_empty() {
        vec![ContentBlock::text(String::new())]
    } else {
        blocks
    }
}

/// One `tools/call` dispatch, transport-agnostic: `(original_name, args)`
/// → the server's `CallResult`.
pub(super) type McpCallFn = Arc<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<CallResult, McpError>> + Send>>
        + Send
        + Sync,
>;

/// Wrap one MCP tool as a conga [`ToolDefinition`], shared by the stdio and
/// HTTP transports: name prefixed `mcp__<server>__<tool>` (server tool
/// names can collide with built-ins or each other), risk High (an external
/// server is unvetted code), execution dispatched through `call`.
pub(super) fn mcp_tool_definition(
    server_name: &str,
    t: McpTool,
    call: McpCallFn,
) -> ToolDefinition {
    let original_name = t.name.clone();
    let prefixed_name = format!("mcp__{server_name}__{}", t.name);
    let label = format!("{}/{}", server_name, t.title.unwrap_or(t.name));
    ToolDefinition {
        name: prefixed_name,
        label,
        description: t.description,
        parameters: t.input_schema,
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx| {
            let call = Arc::clone(&call);
            let original_name = original_name.clone();
            Box::pin(async move {
                if ctx.aborted() {
                    return Ok(ToolResult::error("aborted"));
                }
                match call(original_name, ctx.args).await {
                    Ok(resp) => Ok(ToolResult {
                        content: content_to_blocks(&resp.content),
                        details: serde_json::Value::Null,
                        is_error: resp.is_error,
                    }),
                    Err(e) => Err(ToolError::Message(e.to_string())),
                }
            })
        }),
    }
}

/// Parsed JSON-RPC message (shared by stdio and HTTP paths).
pub(super) enum Parsed {
    Result(serde_json::Value),
    Error { code: i64, message: String },
    Other,
}

/// Parse a JSON-RPC value, matching by `id`.
pub(super) fn parse_jsonrpc_value(v: &serde_json::Value, expected_id: u64) -> Parsed {
    let Some(id_val) = v.get("id") else {
        return Parsed::Other; // notification
    };
    let id = id_val.as_u64().unwrap_or(u64::MAX);
    if id != expected_id {
        return Parsed::Other;
    }
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        Parsed::Error { code, message }
    } else if v.get("result").is_some() {
        Parsed::Result(v["result"].clone())
    } else {
        Parsed::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_maps_text_and_image() {
        let items = vec![
            McpContent {
                kind: "text".into(),
                text: Some("hello".into()),
                data: None,
                mime_type: None,
            },
            McpContent {
                kind: "image".into(),
                text: None,
                data: Some("base64data".into()),
                mime_type: Some("image/png".into()),
            },
        ];
        let blocks = content_to_blocks(&items);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"));
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert_eq!(
                    text,
                    "[image content omitted: image/png, 10 bytes of base64]"
                );
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn content_empty_vec_gives_one_empty_text() {
        let blocks = content_to_blocks(&[]);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.is_empty()));
    }

    #[test]
    fn content_unknown_type_degrades_to_text() {
        let items = vec![McpContent {
            kind: "audio".into(),
            text: None,
            data: Some("audio-data".into()),
            mime_type: Some("audio/wav".into()),
        }];
        let blocks = content_to_blocks(&items);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn parse_jsonrpc_value_matches_result_by_id() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"tools":[]}});
        match parse_jsonrpc_value(&v, 3) {
            Parsed::Result(r) => assert!(r["tools"].is_array()),
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn parse_jsonrpc_value_matches_error_by_id() {
        let v =
            serde_json::json!({"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"nope"}});
        match parse_jsonrpc_value(&v, 5) {
            Parsed::Error { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn parse_jsonrpc_value_skips_unmatched_id_and_notifications() {
        // Wrong id
        let v = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}});
        assert!(matches!(parse_jsonrpc_value(&v, 99), Parsed::Other));

        // Notification (no id)
        let v = serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress"});
        assert!(matches!(parse_jsonrpc_value(&v, 1), Parsed::Other));
    }

    #[test]
    fn parse_jsonrpc_value_defaults_missing_error_fields() {
        // An error object without code/message → the documented defaults.
        let v = serde_json::json!({"jsonrpc":"2.0","id":3,"error":{}});
        match parse_jsonrpc_value(&v, 3) {
            Parsed::Error { code, message } => {
                assert_eq!(code, -1);
                assert_eq!(message, "unknown error");
            }
            _ => panic!("expected Error"),
        }

        // Neither result nor error → Other (nothing usable).
        let v = serde_json::json!({"jsonrpc":"2.0","id":3});
        assert!(matches!(parse_jsonrpc_value(&v, 3), Parsed::Other));
    }

    // ── `mcp_tool_definition` ─────────────────────────────────────

    /// A tool as the MCP wire format describes it.
    fn tool(name: &str, title: Option<&str>) -> McpTool {
        McpTool {
            name: name.to_string(),
            title: title.map(|t| t.to_string()),
            description: "echo back the arguments".to_string(),
            annotations: serde_json::json!({}),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    /// A call fn that records the (name, args) it was invoked with and
    /// answers with one text content block.
    fn recording_call(
        seen: Arc<std::sync::Mutex<Option<(String, serde_json::Value)>>>,
        text: &str,
        is_error: bool,
    ) -> McpCallFn {
        let text = text.to_string();
        Arc::new(move |name: String, args: serde_json::Value| {
            let seen = Arc::clone(&seen);
            let text = text.clone();
            Box::pin(async move {
                *seen.lock().unwrap() = Some((name, args));
                Ok(CallResult {
                    content: vec![McpContent {
                        kind: "text".into(),
                        text: Some(text),
                        data: None,
                        mime_type: None,
                    }],
                    is_error,
                })
            })
        })
    }

    #[tokio::test]
    async fn mcp_tool_definition_maps_metadata_and_prefixes_the_name() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(String, serde_json::Value)>));
        let call = recording_call(Arc::clone(&seen), "hi", false);
        let def = mcp_tool_definition("github", tool("search", Some("Search repos")), call);

        assert_eq!(def.name, "mcp__github__search");
        assert_eq!(def.label, "github/Search repos");
        assert_eq!(def.description, "echo back the arguments");
        assert_eq!(def.parameters, serde_json::json!({"type": "object"}));
        assert_eq!(def.risk, RiskLevel::High);

        let result = (def.execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"q": "conga"}),
        ))
        .await
        .expect("execute");
        assert!(!result.is_error);
        assert!(matches!(
            &result.content[0],
            ContentBlock::Text { text } if text == "hi"
        ));
        // The un-prefixed tool name and the raw args reach the server.
        let seen = seen.lock().unwrap().clone().expect("call happened");
        assert_eq!(seen.0, "search");
        assert_eq!(seen.1, serde_json::json!({"q": "conga"}));
    }

    #[tokio::test]
    async fn mcp_tool_definition_label_falls_back_to_the_tool_name() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(String, serde_json::Value)>));
        let call = recording_call(Arc::clone(&seen), "unused", false);
        let def = mcp_tool_definition("fs", tool("read_file", None), call);
        assert_eq!(def.label, "fs/read_file");
    }

    #[tokio::test]
    async fn mcp_tool_definition_propagates_the_server_error() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(String, serde_json::Value)>));
        let call: McpCallFn = Arc::new(|_name: String, _args: serde_json::Value| {
            Box::pin(async {
                Err(McpError::ServerError {
                    code: -32601,
                    message: "no such tool".into(),
                })
            })
        });
        let def = mcp_tool_definition("fs", tool("read_file", None), call);

        let err = (def.execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({}),
        ))
        .await
        .expect_err("server error must surface");
        assert!(err.to_string().contains("no such tool"), "{err}");
        assert!(seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn mcp_tool_definition_returns_an_error_result_when_aborted() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(String, serde_json::Value)>));
        let call = recording_call(Arc::clone(&seen), "hi", false);
        let def = mcp_tool_definition("fs", tool("read_file", None), call);

        let mut ctx = super::super::test_util::tool_call_ctx_for_test("c1", serde_json::json!({}));
        ctx.signal = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let result = (def.execute)(ctx).await.expect("execute");
        assert!(result.is_error, "an aborted call must be an error result");
        assert!(matches!(
            &result.content[0],
            ContentBlock::Text { text } if text == "aborted"
        ));
        assert!(
            seen.lock().unwrap().is_none(),
            "an aborted call must not reach the server"
        );
    }
}
