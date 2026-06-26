//! RPC dispatcher for plugins.
//!
//! This module provides the dispatcher that routes JSON-RPC method calls to
//! registered handlers with permission enforcement. It's the core routing layer
//! that connects external script processes to Gasket's engine capabilities.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::ToolRegistry;
use gasket_providers::LlmProvider;

use super::manifest::Permission;
use super::rpc::{RpcError, RpcRequest, RpcResponse};

mod llm_chat;
mod subagent;

use crate::tools::ToolContext;
use llm_chat::LlmChatHandler;
use subagent::SubagentSpawnHandler;

/// Trait for RPC method handlers.
///
/// Handlers are registered with the dispatcher and invoked when matching
/// method names are called. Each handler declares its required permission
/// which is checked before execution.
#[async_trait]
pub trait RpcHandler: Send + Sync {
    /// Get the method name this handler responds to.
    fn method(&self) -> &str;

    /// Get the permission required to call this handler.
    fn required_permission(&self) -> Permission;

    /// Handle an RPC request.
    ///
    /// # Arguments
    ///
    /// * `params` - The parameters from the RPC request (can be null/array/object)
    /// * `ctx` - The dispatcher context with engine capabilities
    ///
    /// # Returns
    ///
    /// - `Ok(Value)` - Successful result (will be wrapped in RpcResponse)
    /// - `Err(RpcError)` - Error response
    async fn handle(&self, params: Value, ctx: &DispatcherContext) -> Result<Value, RpcError>;
}

/// Static engine resources that are injected once at construction time.
///
/// These do not change between calls and belong to the plugin's lifecycle.
#[derive(Clone)]
pub struct EngineResources {
    /// Tool registry for executing engine tools
    pub tool_registry: Arc<ToolRegistry>,

    /// LLM provider for direct chat completions
    pub provider: Arc<dyn LlmProvider>,
}

/// Unified handle to engine capabilities.
///
/// Wraps all engine resources that RPC handlers may need.
/// Using a single struct eliminates Option 泛滥 in DispatcherContext.
#[derive(Clone)]
pub struct EngineHandle {
    /// Session identifier for the current session
    pub session_key: gasket_types::events::SessionKey,

    /// Channel for sending outbound messages
    pub outbound_tx: tokio::sync::mpsc::Sender<gasket_types::events::OutboundMessage>,

    /// Subagent spawner. None in contexts that don't support subagents (CLI, tests).
    pub spawner: Option<Arc<dyn gasket_types::SubagentSpawner>>,

    /// Token usage tracker for LLM calls
    pub token_tracker: Arc<gasket_types::token_tracker::TokenTracker>,

    /// Tool registry for executing engine tools
    pub tool_registry: Arc<ToolRegistry>,

    /// LLM provider for direct chat completions
    pub provider: Arc<dyn LlmProvider>,

    /// Pending-ask registry. None in contexts that don't support user prompting.
    pub pending_asks: Option<gasket_types::pending_ask::DynPendingAskRegistry>,
}

/// Context provided to RPC handlers during execution.
///
/// Contains a handle to engine capabilities.
/// Handlers use this to access engine resources.
#[derive(Clone)]
pub struct DispatcherContext {
    /// Engine capabilities handle
    pub engine: Arc<EngineHandle>,
}

/// RPC dispatcher that routes method calls to handlers with permission checks.
///
/// The dispatcher maintains a registry of method handlers and provides:
/// - Method registration and lookup
/// - Permission validation before execution
/// - Standard error responses for common failures
#[derive(Clone)]
pub struct RpcDispatcher {
    /// Registered handlers indexed by method name
    handlers: HashMap<String, Arc<dyn RpcHandler>>,
}

impl RpcDispatcher {
    /// Create a new empty dispatcher.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler for a specific method.
    ///
    /// # Arguments
    ///
    /// * `handler` - The handler to register
    ///
    /// # Returns
    ///
    /// `Err(method_name)` if a handler is already registered for the same method.
    pub fn register(&mut self, handler: Arc<dyn RpcHandler>) -> Result<(), String> {
        let method = handler.method().to_string();
        if self.handlers.contains_key(&method) {
            return Err(method.clone());
        }
        self.handlers.insert(method, handler);
        Ok(())
    }

    /// Dispatch an RPC request to the appropriate handler.
    ///
    /// # Arguments
    ///
    /// * `request` - The RPC request to dispatch
    /// * `permissions` - Permissions granted to the calling script
    /// * `ctx` - The dispatcher context
    ///
    /// # Returns
    ///
    /// An RPC response. For notifications (id=None), the response id will be Value::Null.
    ///
    /// # Error Handling
    ///
    /// - Method not found: returns error code -32601
    /// - Permission denied: returns error code -32000
    /// - Handler error: returns error from handler
    pub async fn dispatch(
        &self,
        request: RpcRequest,
        permissions: &[Permission],
        ctx: &DispatcherContext,
    ) -> RpcResponse {
        // Extract request id (use Value::Null for notifications)
        let id = request.id.clone().unwrap_or(Value::Null);

        // Find handler by method name
        let handler = match self.handlers.get(&request.method) {
            Some(h) => h,
            None => {
                return RpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: None,
                    error: Some(RpcError::method_not_found(&request.method)),
                };
            }
        };

        // Check permissions
        let required = handler.required_permission();
        if !permissions.contains(&required) {
            return RpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(RpcError::permission_denied(&request.method)),
            };
        }

        // Execute handler
        let params = request.params.unwrap_or(Value::Null);
        match handler.handle(params, ctx).await {
            Ok(result) => RpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => RpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(error),
            },
        }
    }
}

impl Default for RpcDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Build and return an RPC dispatcher with default handlers.
///
/// Registers all built-in callback handlers for plugin RPC methods:
/// - `llm/chat` - LLM chat completions
/// - `subagent/spawn` - Subagent spawning
///   Generic handler that delegates an RPC method to an engine tool.
pub struct ToolDelegateHandler {
    method: &'static str,
    permission: Permission,
    tool_name: &'static str,
}

impl ToolDelegateHandler {
    pub fn new(method: &'static str, permission: Permission, tool_name: &'static str) -> Self {
        Self {
            method,
            permission,
            tool_name,
        }
    }
}

#[async_trait]
impl RpcHandler for ToolDelegateHandler {
    fn method(&self) -> &str {
        self.method
    }

    fn required_permission(&self) -> Permission {
        self.permission.clone()
    }

    async fn handle(&self, params: Value, ctx: &DispatcherContext) -> Result<Value, RpcError> {
        let mut tool_ctx = ToolContext::default()
            .session_key(ctx.engine.session_key.clone())
            .outbound_tx(ctx.engine.outbound_tx.clone())
            .token_tracker(ctx.engine.token_tracker.clone());

        if let Some(spawner) = &ctx.engine.spawner {
            tool_ctx = tool_ctx.spawner(spawner.clone());
        }

        if let Some(registry) = &ctx.engine.pending_asks {
            tool_ctx = tool_ctx.pending_asks(registry.clone());
        }

        // Inject SynthesisCallback for streaming channels
        if ctx.engine.session_key.channel.supports_streaming() {
            let model = ctx.engine.provider.default_model().to_string();
            let callback =
                std::sync::Arc::new(crate::kernel::synthesis::WebSocketSynthesizer::new(
                    ctx.engine.provider.clone(),
                    model,
                    ctx.engine.outbound_tx.clone(),
                    ctx.engine.session_key.clone(),
                ));
            tool_ctx = tool_ctx.synthesis_callback(callback);
        }

        let output = ctx
            .engine
            .tool_registry
            .execute(self.tool_name, params, &tool_ctx)
            .await
            .map_err(|e| RpcError::internal_error(format!("{} failed: {}", self.tool_name, e)))?;

        Ok(serde_json::json!({"output": output}))
    }
}

pub fn build_dispatcher() -> RpcDispatcher {
    let mut d = RpcDispatcher::new();
    // Unwrap is safe: built-in handlers have unique method names.
    d.register(Arc::new(LlmChatHandler)).unwrap();
    d.register(Arc::new(SubagentSpawnHandler)).unwrap();
    d.register(Arc::new(ToolDelegateHandler::new(
        "message/send",
        Permission::MessageSend,
        "send_message",
    )))
    .unwrap();
    d.register(Arc::new(ToolDelegateHandler::new(
        "user/ask",
        Permission::UserAsk,
        "ask_user",
    )))
    .unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use gasket_types::events::SessionKey;
    use gasket_types::{SubagentResult, SubagentSpawner};
    use serde_json::json;
    use std::sync::Arc;

    struct MockSpawner;
    #[async_trait::async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn(
            &self,
            _task: String,
            _model_id: Option<String>,
            _ctx: &crate::tools::ToolContext,
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

    pub struct MockProvider;
    #[async_trait::async_trait]
    impl gasket_providers::LlmProvider for MockProvider {
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
            Ok(gasket_providers::ChatResponse {
                content: Some("Test response".to_string()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(gasket_providers::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                }),
            })
        }
    }

    fn create_test_ctx() -> DispatcherContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        DispatcherContext {
            engine: Arc::new(EngineHandle {
                session_key: SessionKey::new(
                    gasket_types::events::ChannelType::Telegram,
                    "test-chat",
                ),
                outbound_tx: tx,
                spawner: Some(Arc::new(MockSpawner)),
                token_tracker: Arc::new(gasket_types::token_tracker::TokenTracker::unlimited(
                    "USD",
                )),
                tool_registry: Arc::new(ToolRegistry::new()),
                provider: Arc::new(MockProvider),
                pending_asks: None,
            }),
        }
    }

    struct EchoHandler {
        method: String,
        permission: Permission,
    }

    #[async_trait]
    impl RpcHandler for EchoHandler {
        fn method(&self) -> &str {
            &self.method
        }

        fn required_permission(&self) -> Permission {
            self.permission.clone()
        }

        async fn handle(&self, params: Value, _ctx: &DispatcherContext) -> Result<Value, RpcError> {
            Ok(json!({"echo": params}))
        }
    }

    #[tokio::test]
    async fn test_dispatch_success() {
        let mut dispatcher = RpcDispatcher::new();
        let handler = Arc::new(EchoHandler {
            method: "test/echo".to_string(),
            permission: Permission::LlmChat,
        });
        dispatcher.register(handler).unwrap();

        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "test/echo".to_string(),
            params: Some(json!({"hello": "world"})),
        };

        let permissions = vec![Permission::LlmChat];
        let ctx = create_test_ctx();
        let response = dispatcher.dispatch(request, &permissions, &ctx).await;

        assert_eq!(response.id, json!(1));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert_eq!(
            response.result.unwrap(),
            json!({"echo": {"hello": "world"}})
        );
    }

    #[tokio::test]
    async fn test_dispatch_permission_denied() {
        let mut dispatcher = RpcDispatcher::new();
        let handler = Arc::new(EchoHandler {
            method: "test/echo".to_string(),
            permission: Permission::LlmChat,
        });
        dispatcher.register(handler).unwrap();

        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(2)),
            method: "test/echo".to_string(),
            params: Some(json!({"test": "data"})),
        };

        let permissions = vec![];
        let ctx = create_test_ctx();
        let response = dispatcher.dispatch(request, &permissions, &ctx).await;

        assert_eq!(response.id, json!(2));
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        let error = response.error.unwrap();
        assert_eq!(error.code, -32000);
        assert!(error.message.contains("Permission denied"));
    }

    #[tokio::test]
    async fn test_dispatch_method_not_found() {
        let dispatcher = RpcDispatcher::new();
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(3)),
            method: "unknown/method".to_string(),
            params: None,
        };
        let permissions = vec![Permission::LlmChat];
        let ctx = create_test_ctx();
        let response = dispatcher.dispatch(request, &permissions, &ctx).await;

        assert_eq!(response.id, json!(3));
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        let error = response.error.unwrap();
        assert_eq!(error.code, -32601);
        assert!(error.message.contains("Method not found"));
    }

    #[tokio::test]
    async fn test_dispatch_no_id() {
        let mut dispatcher = RpcDispatcher::new();
        let handler = Arc::new(EchoHandler {
            method: "test/notify".to_string(),
            permission: Permission::LlmChat,
        });
        dispatcher.register(handler).unwrap();

        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "test/notify".to_string(),
            params: Some(json!({"event": "test"})),
        };

        let permissions = vec![Permission::LlmChat];
        let ctx = create_test_ctx();
        let response = dispatcher.dispatch(request, &permissions, &ctx).await;

        assert_eq!(response.id, Value::Null);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap(), json!({"echo": {"event": "test"}}));
    }
}
