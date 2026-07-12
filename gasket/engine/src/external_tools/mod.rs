//! Plugins module
//!
//! This module provides support for external plugins that can be
//! integrated into Gasket via YAML manifests. Plugins communicate via
//! either Simple (stdin/stdout JSON) or JSON-RPC 2.0 protocols.

pub mod dispatcher;
pub mod manifest;
pub mod rpc;
pub mod runner;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
pub use dispatcher::{
    build_dispatcher, DispatcherContext, EngineHandle, EngineResources, RpcDispatcher,
};
pub use manifest::{Permission, PluginManifest, PluginProtocol, RuntimeConfig};
pub use runner::{run_simple, JsonRpcDaemon, PluginError, PluginResult};

/// Plugin that implements the Tool trait for external scripts.
///
/// PluginTool wraps an external script with a YAML manifest and exposes
/// it as a native Gasket tool. The `runtime` field is an enum that captures
/// exactly the state needed by each protocol — no `Option` clusters, no
/// "this should never happen" runtime checks.
#[derive(Clone)]
pub struct PluginTool {
    /// Parsed manifest describing the script
    manifest: PluginManifest,
    /// Directory containing the manifest (for resolving paths)
    manifest_dir: PathBuf,
    /// Protocol-specific runtime state.
    runtime: PluginRuntime,
}

/// Protocol-specific runtime state for a `PluginTool`.
///
/// One variant per `PluginProtocol`. Each variant carries exactly the
/// fields its protocol needs — no shared `Option` baggage.
#[derive(Clone)]
enum PluginRuntime {
    /// Simple stdin/stdout JSON. Stateless; no daemon, no dispatcher.
    Simple,
    /// JSON-RPC 2.0 over stdio. Holds dispatcher + engine resources +
    /// lazy daemon handle, all required fields, never `None`.
    JsonRpc(JsonRpcState),
}

/// Runtime state for a JSON-RPC plugin.
#[derive(Clone)]
struct JsonRpcState {
    dispatcher: Arc<RpcDispatcher>,
    resources: EngineResources,
    /// Lazy daemon: `None` until first call, then populated and reused
    /// until idle-expired. Uses a simple parking_lot::Mutex — no async
    /// lock, no double-checked locking dance. Plugins are low-concurrency.
    daemon: Arc<parking_lot::Mutex<Option<Arc<JsonRpcDaemon>>>>,
}

impl PluginTool {
    /// Get the underlying manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Create a new PluginTool from a manifest.
    ///
    /// JSON-RPC plugins require `resources` to be `Some`. If a manifest
    /// declares `protocol: json_rpc` but no engine resources are supplied,
    /// the plugin is downgraded to Simple mode (with a warning) so that
    /// discovery doesn't fail outright — callers can still execute it,
    /// they just don't get JSON-RPC callbacks.
    pub fn new(
        manifest: PluginManifest,
        manifest_dir: PathBuf,
        resources: Option<EngineResources>,
    ) -> Self {
        let runtime = match (manifest.protocol, resources) {
            (PluginProtocol::JsonRpc, Some(resources)) => PluginRuntime::JsonRpc(JsonRpcState {
                dispatcher: Arc::new(build_dispatcher()),
                resources,
                daemon: Arc::new(parking_lot::Mutex::new(None)),
            }),
            (PluginProtocol::JsonRpc, None) => {
                warn!(
                    "Plugin '{}' declares protocol=json_rpc but no engine resources \
                     were supplied; falling back to simple mode",
                    manifest.name
                );
                PluginRuntime::Simple
            }
            (PluginProtocol::Simple, _) => PluginRuntime::Simple,
        };
        Self {
            manifest,
            manifest_dir,
            runtime,
        }
    }
}

impl JsonRpcState {
    /// Build a dispatcher context from a tool context.
    fn make_dispatch_ctx(&self, ctx: &ToolContext) -> DispatcherContext {
        DispatcherContext {
            engine: Arc::new(EngineHandle {
                session_key: ctx.session_key.clone(),
                outbound_tx: ctx.outbound_tx.clone(),
                spawner: ctx.spawner.clone(),
                token_tracker: ctx.token_tracker.clone(),
                tool_registry: self.resources.tool_registry.clone(),
                provider: self.resources.provider.clone(),
                pending_asks: ctx.pending_asks.clone(),
            }),
        }
    }

    /// Get or spawn the JSON-RPC daemon, handling idle expiration.
    ///
    /// Uses a simple synchronous `parking_lot::Mutex` — no async lock, no
    /// double-checked locking. Plugin calls are serial/low-concurrency so
    /// the brief mutex hold during spawn-amortization is irrelevant.
    async fn get_or_spawn_daemon(
        &self,
        manifest: &PluginManifest,
        manifest_dir: &Path,
        dispatch_ctx: &DispatcherContext,
        call_timeout_secs: u64,
        idle_timeout_secs: u64,
    ) -> Result<Arc<JsonRpcDaemon>, ToolError> {
        // Fast path: existing live daemon (no idle expiry).
        {
            let guard = self.daemon.lock();
            if let Some(d) = guard.as_ref() {
                if !d.is_idle_expired() {
                    return Ok(d.clone());
                }
            }
        }

        // Slow path: spawn new daemon. The spawn itself is async, so we
        // can't hold the non-Send parking_lot::MutexGuard across .await.
        // Instead: acquire lock to check, release, spawn, acquire again to set.
        let new_daemon = Arc::new(
            JsonRpcDaemon::spawn(
                manifest,
                manifest_dir,
                call_timeout_secs,
                idle_timeout_secs,
                &manifest.permissions,
                &self.dispatcher,
                dispatch_ctx,
            )
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?,
        );

        {
            let mut guard = self.daemon.lock();
            *guard = Some(new_daemon.clone());
        }
        Ok(new_daemon)
    }
}

#[async_trait]
impl Tool for PluginTool {
    /// Get the tool name from the manifest.
    fn name(&self) -> &str {
        &self.manifest.name
    }

    /// Get the tool description from the manifest.
    fn description(&self) -> &str {
        &self.manifest.description
    }

    /// Get the JSON schema for parameters from the manifest.
    fn parameters(&self) -> Value {
        self.manifest.parameters.clone()
    }

    /// Execute the plugin.
    ///
    /// # Protocol
    ///
    /// - **Simple mode**: One-shot execution with JSON input/output
    /// - **JSON-RPC mode**: Bidirectional communication with method calls
    ///
    /// # Returns
    ///
    /// - `Ok(String)` - JSON-encoded result with optional `_debug_stderr` field
    /// - `Err(ToolError)` - Spawn, timeout, exit, or protocol error
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let start = std::time::Instant::now();
        let call_timeout_secs = self
            .manifest
            .runtime
            .timeout_secs
            .unwrap_or(ctx.plugin_timeout_secs);

        let result = match &self.runtime {
            PluginRuntime::Simple => {
                run_simple(&self.manifest, &self.manifest_dir, &args, call_timeout_secs).await
            }
            PluginRuntime::JsonRpc(state) => {
                let dispatch_ctx = state.make_dispatch_ctx(ctx);
                // Default idle = 4 × call timeout: keep the daemon warm across
                // a few quiet calls without pinning the process forever.
                let idle_timeout_secs = self
                    .manifest
                    .runtime
                    .idle_timeout_secs
                    .unwrap_or(call_timeout_secs.saturating_mul(4));
                let daemon = state
                    .get_or_spawn_daemon(
                        &self.manifest,
                        &self.manifest_dir,
                        &dispatch_ctx,
                        call_timeout_secs,
                        idle_timeout_secs,
                    )
                    .await?;
                // Inject default model so the SDK can fall back when plugin omits it
                let mut init_args = args.clone();
                let default_model = dispatch_ctx.engine.provider.default_model();
                if let Some(obj) = init_args.as_object_mut() {
                    obj.insert("_gasket_default_model".to_string(), default_model.into());
                    obj.insert(
                        "_gasket_channel".to_string(),
                        ctx.session_key.channel.to_string().into(),
                    );
                    obj.insert(
                        "_gasket_chat_id".to_string(),
                        ctx.session_key.chat_id.clone().into(),
                    );
                } else {
                    init_args = serde_json::json!({
                        "_gasket_default_model": default_model,
                        "_gasket_channel": ctx.session_key.channel.to_string(),
                        "_gasket_chat_id": ctx.session_key.chat_id.clone(),
                    });
                }
                daemon.call("initialize", Some(init_args)).await
            }
        };

        let duration = start.elapsed();

        match result {
            Ok(script_result) => {
                // Build output object
                let mut output = serde_json::Map::new();
                output.insert("result".to_string(), script_result.output);

                // Attach stderr as _debug_stderr field in JSON-RPC mode
                if matches!(self.runtime, PluginRuntime::JsonRpc(_))
                    && !script_result.stderr.is_empty()
                {
                    output.insert("_debug_stderr".to_string(), script_result.stderr.into());
                }

                // Add duration metadata
                output.insert(
                    "_duration_ms".to_string(),
                    (duration.as_millis() as u64).into(),
                );

                // Serialize to JSON string
                serde_json::to_string(&Value::Object(output)).map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to serialize result: {}", e))
                })
            }
            Err(e) => Err(ToolError::ExecutionError(format!(
                "Plugin '{}' error: {}",
                self.manifest.name, e
            ))),
        }
    }
}

/// Discover and load all plugins in a directory.
///
/// Scans the directory for `*.yaml` files, parses them as manifests,
/// and creates PluginTool instances for valid manifests.
///
/// # Arguments
///
/// * `plugins_dir` - Directory to scan for manifest files
///
/// # Returns
///
/// * `Ok(Vec<PluginTool>)` - Vector of discovered plugins
/// * `Err(anyhow::Error)` - Directory read error or manifest parse error
pub fn discover_plugins_in_dir(
    plugins_dir: &Path,
    engine: Option<EngineResources>,
) -> anyhow::Result<Vec<PluginTool>> {
    let mut tools = Vec::new();

    // Check if directory exists
    if !plugins_dir.exists() {
        info!(
            "Plugins directory does not exist: {:?}, skipping discovery",
            plugins_dir
        );
        return Ok(tools);
    }

    // Read directory entries
    let entries = std::fs::read_dir(plugins_dir).map_err(|e| {
        anyhow::anyhow!("Failed to read plugins directory {:?}: {}", plugins_dir, e)
    })?;

    // Process each YAML file
    for entry in entries {
        let entry = entry.map_err(|e| {
            anyhow::anyhow!("Failed to read directory entry in {:?}: {}", plugins_dir, e)
        })?;

        let path = entry.path();

        // Skip directories and non-YAML files
        if path.is_dir() {
            continue;
        }

        let ext = path.extension().and_then(|s| s.to_str());
        if ext != Some("yaml") && ext != Some("yml") {
            continue;
        }

        // Load manifest — inject engine resources at construction time
        match load_manifest(&path) {
            Ok(manifest) => {
                info!("Discovered plugin '{}' from {:?}", manifest.name, path);
                let tool = PluginTool::new(manifest, plugins_dir.to_path_buf(), engine.clone());
                tools.push(tool);
            }
            Err(e) => {
                warn!("Failed to load manifest from {:?}: {}", path, e);
            }
        }
    }

    Ok(tools)
}

/// Discover and register all plugins in the Gasket plugins directory.
///
/// This is the main entry point for plugin discovery. It reads
/// `~/.gasket/plugins/` and registers all valid manifests with the
/// provided tool registry.
///
/// # Arguments
///
/// * `registry` - Tool registry to register discovered tools
/// * `engine` - Optional engine resources for JSON-RPC plugins.
///   If provided, JSON-RPC plugins are pre-configured with engine resources.
///   If `None`, they are still registered but require `link_engine_refs()`
///   before they can be executed.
///
/// # Returns
///
/// * `Ok(())` - Discovery completed successfully
/// * `Err(anyhow::Error)` - Directory read or manifest parse error
///
/// # Note
///
/// If the plugins directory does not exist, this function returns `Ok(())`
/// without error. Missing directories are treated as empty tool sets.
pub fn discover_plugins(
    registry: &mut ToolRegistry,
    engine: Option<EngineResources>,
) -> anyhow::Result<()> {
    // Resolve plugins directory: ~/.gasket/plugins/
    let plugins_dir = dirs::home_dir()
        .map(|home| home.join(".gasket/plugins"))
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve home directory"))?;

    // Discover tools — engine resources are injected at construction time.
    let tools = discover_plugins_in_dir(&plugins_dir, engine)?;

    for tool in tools {
        registry.register(Box::new(tool));
    }

    info!("Discovered and registered plugins from {:?}", plugins_dir);

    Ok(())
}

/// Load a script manifest from a YAML file.
///
/// # Arguments
///
/// * `path` - Path to the manifest file
///
/// # Returns
///
/// * `Ok(PluginManifest)` - Parsed manifest
/// * `Err(anyhow::Error)` - File read or YAML parse error
fn load_manifest(path: &Path) -> anyhow::Result<PluginManifest> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest file {:?}: {}", path, e))?;

    let manifest: PluginManifest = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse manifest YAML from {:?}: {}", path, e))?;

    // Validate manifest
    if manifest.name.is_empty() {
        return Err(anyhow::anyhow!("Manifest from {:?} has empty name", path));
    }

    if manifest.description.is_empty() {
        return Err(anyhow::anyhow!(
            "Manifest from {:?} has empty description",
            path
        ));
    }

    if manifest.runtime.command.is_empty() {
        return Err(anyhow::anyhow!(
            "Manifest from {:?} has empty command",
            path
        ));
    }

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// Create a minimal manifest for testing.
    fn test_manifest(command: &str) -> (PluginManifest, TempDir) {
        let dir = TempDir::new().unwrap();
        let manifest = PluginManifest {
            name: "test_tool".to_string(),
            description: "Test tool".to_string(),
            version: "1.0.0".to_string(),
            runtime: RuntimeConfig {
                command: command.to_string(),
                args: vec![],
                working_dir: ".".to_string(),
                timeout_secs: Some(120),
                idle_timeout_secs: None,
                env: Default::default(),
            },
            protocol: PluginProtocol::Simple,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            }),
            permissions: vec![],
        };
        (manifest, dir)
    }

    #[test]
    fn test_script_tool_new() {
        let (manifest, dir) = test_manifest("cat");
        let tool = PluginTool::new(manifest, dir.path().to_path_buf(), None);

        assert_eq!(tool.name(), "test_tool");
        assert_eq!(tool.description(), "Test tool");
        assert_eq!(tool.parameters()["type"], "object");
    }

    #[tokio::test]
    async fn test_simple_tool_execute() {
        let (manifest, dir) = test_manifest("cat");
        let tool = PluginTool::new(manifest, dir.path().to_path_buf(), None);

        let args = json!({"hello": "world", "number": 42});
        let ctx = ToolContext::default();

        let result = tool.execute(args, &ctx).await;

        assert!(result.is_ok());
        let output_str = result.unwrap();
        let output: Value = serde_json::from_str(&output_str).unwrap();

        // Verify result field contains echoed input
        assert_eq!(output["result"]["hello"], "world");
        assert_eq!(output["result"]["number"], 42);

        // Verify metadata fields
        assert!(output["_duration_ms"].is_number());
    }

    #[test]
    fn test_discover_plugins_no_dir() {
        // Use a nonexistent directory path
        let nonexistent = PathBuf::from("/tmp/nonexistent_gasket_scripts_xyz123");

        let tools = discover_plugins_in_dir(&nonexistent, None).unwrap();

        // Should return empty vec without error
        assert_eq!(tools.len(), 0);
    }

    #[test]
    fn test_load_manifest_invalid_yaml() {
        let dir = TempDir::new().unwrap();
        let manifest_path = dir.path().join("invalid.yaml");

        // Write invalid YAML
        std::fs::write(&manifest_path, "invalid: yaml: content: [").unwrap();

        let result = load_manifest(&manifest_path);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }

    #[test]
    fn test_load_manifest_missing_command() {
        let dir = TempDir::new().unwrap();
        let manifest_path = dir.path().join("no_command.yaml");

        let yaml = r#"
name: "test_tool"
description: "Tool with no command"
runtime:
  command: ""
parameters:
  type: "object"
  properties: {}
"#;

        std::fs::write(&manifest_path, yaml).unwrap();

        let result = load_manifest(&manifest_path);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty command"));
    }

    #[test]
    fn test_script_tool_make_dispatch_ctx() {
        use gasket_providers::LlmProvider;
        use gasket_types::{
            token_tracker::TokenTracker, ChannelType, OutboundMessage, SessionKey, SubagentResult,
            SubagentSpawner,
        };
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

        struct MockProvider;
        #[async_trait::async_trait]
        impl LlmProvider for MockProvider {
            fn name(&self) -> &str {
                "mock"
            }
            fn default_model(&self) -> &str {
                "mock-model"
            }
            async fn chat(
                &self,
                _request: gasket_providers::ChatRequest,
            ) -> Result<gasket_providers::ChatResponse, gasket_providers::ProviderError>
            {
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

        let dir = TempDir::new().unwrap();
        let manifest = PluginManifest {
            name: "test_tool".to_string(),
            description: "Test tool".to_string(),
            version: "1.0.0".to_string(),
            runtime: RuntimeConfig {
                command: "cat".to_string(),
                args: vec![],
                working_dir: ".".to_string(),
                timeout_secs: Some(120),
                idle_timeout_secs: None,
                env: Default::default(),
            },
            protocol: PluginProtocol::JsonRpc,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            }),
            permissions: vec![],
        };

        let tool = PluginTool::new(
            manifest,
            dir.path().to_path_buf(),
            Some(EngineResources {
                tool_registry: Arc::new(ToolRegistry::new()),
                provider: Arc::new(MockProvider),
            }),
        );

        let (tx, _rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
        let ctx = ToolContext::default()
            .session_key(SessionKey::new(ChannelType::Telegram, "test-chat"))
            .outbound_tx(tx)
            .spawner(Arc::new(MockSpawner))
            .token_tracker(Arc::new(TokenTracker::unlimited("USD")));

        // Verify the JsonRpc runtime variant was constructed and can build
        // a dispatcher context without panicking.
        match &tool.runtime {
            PluginRuntime::JsonRpc(state) => {
                let _dispatch_ctx = state.make_dispatch_ctx(&ctx);
            }
            PluginRuntime::Simple => {
                panic!("Expected JsonRpc runtime, got Simple");
            }
        }
    }

    #[test]
    fn test_json_rpc_without_resources_falls_back_to_simple() {
        let dir = TempDir::new().unwrap();
        let manifest = PluginManifest {
            name: "needs_engine".to_string(),
            description: "Declares JSON-RPC but no engine resources are provided".to_string(),
            version: "1.0.0".to_string(),
            runtime: RuntimeConfig {
                command: "cat".to_string(),
                args: vec![],
                working_dir: ".".to_string(),
                timeout_secs: Some(120),
                idle_timeout_secs: None,
                env: Default::default(),
            },
            protocol: PluginProtocol::JsonRpc,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            permissions: vec![],
        };
        let tool = PluginTool::new(manifest, dir.path().to_path_buf(), None);
        assert!(
            matches!(tool.runtime, PluginRuntime::Simple),
            "JSON-RPC plugin without engine resources should downgrade to Simple"
        );
    }
}
