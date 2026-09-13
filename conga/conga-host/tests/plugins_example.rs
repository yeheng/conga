//! Integration: `conga-ext` register shapes against the agent loop (mock).

use std::sync::Arc;

use conga::{
    AgentContext, AgentLoopConfig, AgentMessage, ContentBlock, ExtensionApiImpl, ModelSpec,
    ProviderApi, StreamChunk, StreamFn, ToolDefinition,
};
use futures_util::{stream, Stream};

struct CallToolOnce {
    tool: String,
    args: serde_json::Value,
}
impl StreamFn for CallToolOnce {
    fn stream(
        &self,
        _model: &ModelSpec,
        _messages: &[AgentMessage],
        _system: &str,
        _tools: &[ToolDefinition],
        _signal: Option<conga::CancelSignal>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let tool = self.tool.clone();
        let args_str = self.args.to_string();
        Box::pin(stream::iter(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "call_1".into(),
                name: Some(tool),
                args_delta: args_str,
            },
            StreamChunk::Done,
        ]))
    }
}

#[tokio::test]
async fn hello_extension_greets() {
    let mut api = ExtensionApiImpl::new();
    conga_ext::hello::register(&mut api);
    let tools = std::mem::take(&mut api.tools);

    let ctx = AgentContext {
        system_prompt: "".into(),
        messages: vec![],
        tools,
        cwd: ".".into(),
        env: Default::default(),
        session_id: "t".into(),
    };
    let cfg = AgentLoopConfig {
        model: ModelSpec {
            id: "m".into(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 64,
        },
        max_turns: 1,
        max_tool_calls_per_turn: 5,
        tool_timeout: None,
        signal: None,
        stream_fn: Arc::new(CallToolOnce {
            tool: "hello".into(),
            args: serde_json::json!({"name": "Ada"}),
        }),
        hooks: None,
        retry: conga::RetryPolicy::default(),
        persist: None,
        steer: None,
        transform_context: None,
    };

    let msgs = conga::agent_loop(vec![], ctx, cfg).await.unwrap();
    let greeted = msgs.iter().any(|m| {
        matches!(m, AgentMessage::ToolResult(tr) if tr.content.iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello, Ada!")))
    });
    assert!(greeted, "hello tool should have greeted Ada");
}
