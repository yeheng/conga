//! Shared JSON-RPC wire types, MCP content mapping, and the tool wrapper
//! used identically by both the stdio and HTTP transports.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use conga::{ContentBlock, RiskLevel, ToolDefinition, ToolError, ToolResult};
use serde::Deserialize;

use super::McpError;

#[derive(Debug, Deserialize)]
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
pub(super) fn mcp_tool_definition(server_name: &str, t: McpTool, call: McpCallFn) -> ToolDefinition {
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
}
