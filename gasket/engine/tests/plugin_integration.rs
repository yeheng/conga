use gasket_engine::external_tools::discover_plugins_in_dir;
use gasket_engine::tools::{Tool, ToolContext, ToolRegistry};
use gasket_providers::LlmProvider;
use gasket_types::{
    token_tracker::TokenTracker, ChannelType, OutboundMessage, SessionKey, SubagentResult,
    SubagentSpawner,
};
use std::path::PathBuf;
use std::sync::Arc;

fn test_scripts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("workspace")
        .join("plugins")
}

struct MockSpawner;
#[async_trait::async_trait]
impl SubagentSpawner for MockSpawner {
    async fn spawn(
        &self,
        _task: String,
        _model_id: Option<String>,
        _ctx: &ToolContext,
    ) -> Result<SubagentResult, Box<dyn std::error::Error + Send>> {
        Ok(SubagentResult {
            id: "mock".to_string(),
            task: "mock".to_string(),
            response: gasket_types::SubagentResponse {
                content: "mock".to_string(),
                reasoning_content: None,
                tools_used: vec![],
                model: None,
                token_usage: None,
                cost: 0.0,
            },
            model: None,
        })
    }
}

struct FailingMockProvider;
#[async_trait::async_trait]
impl LlmProvider for FailingMockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat(
        &self,
        _request: gasket_providers::ChatRequest,
    ) -> Result<gasket_providers::ChatResponse, gasket_providers::ProviderError> {
        Err(gasket_providers::ProviderError::ApiError {
            status_code: 500,
            message: "mock failure".to_string(),
        })
    }
}

fn make_test_ctx() -> ToolContext {
    let (tx, _rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
    ToolContext::default()
        .session_key(SessionKey::new(ChannelType::Telegram, "test-chat"))
        .outbound_tx(tx)
        .spawner(Arc::new(MockSpawner))
        .token_tracker(Arc::new(TokenTracker::unlimited("USD")))
}

#[tokio::test]
async fn test_simple_echo_tool() {
    let tools = discover_plugins_in_dir(&test_scripts_dir(), None).unwrap();
    let echo_tool = tools
        .iter()
        .find(|t| t.name() == "test_echo")
        .expect("test_echo not found");
    let args = serde_json::json!({"message": "hello world"});
    let result = echo_tool.execute(args, &ToolContext::default()).await;
    assert!(result.is_ok(), "Simple echo failed: {:?}", result);
    let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(parsed["result"]["echo"], "hello world");
    assert_eq!(parsed["result"]["status"], "ok");
}

#[tokio::test]
async fn test_jsonrpc_ping_tool() {
    let tools = discover_plugins_in_dir(&test_scripts_dir(), None).unwrap();
    let ping_manifest = tools
        .into_iter()
        .find(|t| t.name() == "test_ping")
        .expect("test_ping not found")
        .manifest()
        .clone();
    let ping_tool = gasket_engine::external_tools::PluginTool::new(
        ping_manifest,
        test_scripts_dir(),
        Some(gasket_engine::external_tools::EngineResources {
            tool_registry: Arc::new(ToolRegistry::new()),
            provider: Arc::new(FailingMockProvider),
        }),
    );
    let args = serde_json::json!({"name": "Alice"});
    let result = ping_tool.execute(args, &make_test_ctx()).await;
    assert!(result.is_ok(), "JsonRpc ping failed: {:?}", result);
    let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(parsed["result"]["greeting"], "Hello, Alice!");
    assert_eq!(parsed["result"]["llm_called"], false);
}

#[test]
fn test_discover_finds_both_tools() {
    let tools = discover_plugins_in_dir(&test_scripts_dir(), None).unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"test_echo"), "Missing test_echo");
    assert!(names.contains(&"test_ping"), "Missing test_ping");
}
