//! `hello` tool — minimum extension shape.

use std::sync::Arc;

use conga::{ContentBlock, ExtensionApi, RiskLevel, ToolDefinition, ToolResult};

pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone. Pass {\"name\": \"...\"}.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Who to greet" }
            },
            "required": ["name"]
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(|ctx| {
            Box::pin(async move {
                let name = ctx.args["name"].as_str().unwrap_or("world");
                Ok(ToolResult {
                    content: vec![ContentBlock::text(format!("Hello, {}!", name))],
                    details: serde_json::json!({ "greeted": name }),
                    is_error: false,
                })
            })
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::{ContentBlock, ExtensionApiImpl, ToolCallCtx, ToolContext};

    fn ctx(args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            args,
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: std::path::PathBuf::from("."),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: std::path::PathBuf::from("."),
            },
        }
    }

    fn text_of(res: &ToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn register_exposes_the_hello_tool() {
        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        assert_eq!(api.tools.len(), 1, "exactly one tool is registered");
        let tool = &api.tools[0];
        assert_eq!(tool.name, "hello");
        assert_eq!(tool.label, "Hello");
        // The tool only formats a greeting: it must stay auto-approvable.
        assert!(matches!(tool.risk, RiskLevel::Low));
        assert_eq!(tool.parameters["type"], "object");
        assert_eq!(tool.parameters["required"][0], "name");
        assert_eq!(tool.parameters["properties"]["name"]["type"], "string");
    }

    #[tokio::test]
    async fn hello_greets_the_given_name() {
        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        let tool = api.tools.remove(0);
        let res = (tool.execute)(ctx(serde_json::json!({ "name": "世界" })))
            .await
            .expect("hello succeeds");
        assert!(!res.is_error);
        assert_eq!(text_of(&res), "Hello, 世界!");
        assert_eq!(res.details["greeted"], "世界");
    }

    #[tokio::test]
    async fn hello_defaults_to_world_when_name_is_missing() {
        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        let tool = api.tools.remove(0);
        // Boundary: no `name` argument at all -> the documented fallback.
        let res = (tool.execute)(ctx(serde_json::json!({})))
            .await
            .expect("hello succeeds");
        assert_eq!(text_of(&res), "Hello, world!");
        assert_eq!(res.details["greeted"], "world");
    }

    #[tokio::test]
    async fn hello_falls_back_to_world_for_a_non_string_name() {
        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        let tool = api.tools.remove(0);
        // Error path: a wrong-typed argument must not panic.
        let res = (tool.execute)(ctx(serde_json::json!({ "name": 42 })))
            .await
            .expect("hello succeeds");
        assert_eq!(text_of(&res), "Hello, world!");
        assert_eq!(res.details["greeted"], "world");
    }
}
