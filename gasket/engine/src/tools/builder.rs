//! Tool registry builder for constructing a fully-configured `ToolRegistry`.
//!
//! This module centralizes tool registration logic so that it lives in the
//! `tools` module rather than being duplicated or scattered across gateway
//! and agent construction sites.

use std::path::Path;
use std::sync::Arc;

use crate::SubagentSpawner;

use super::{CoreToolProvider, SystemToolProvider, ToolProvider};
use super::{Tool, ToolMetadata, ToolRegistry};

/// Resolve a potentially relative path to an absolute path.
fn resolve_to_absolute(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    } else {
        path
    }
}

/// Resolve the exec workspace directory from config or default to `$HOME/.gasket`.
///
/// The returned path is always absolute so that downstream path validators
/// (canonicalize + starts_with) work correctly.
/// Creates the directory if it doesn't exist.
pub fn resolve_exec_workspace(
    config: &crate::config::Config,
    fallback: &Path,
) -> std::path::PathBuf {
    let workspace_path = if let Some(ref ws) = config.tools.exec.workspace {
        resolve_to_absolute(std::path::PathBuf::from(ws))
    } else {
        resolve_to_absolute(
            dirs::home_dir()
                .map(|h| h.join(".gasket"))
                .unwrap_or_else(|| fallback.to_path_buf()),
        )
    };

    if !workspace_path.exists() {
        if let Err(e) = std::fs::create_dir_all(&workspace_path) {
            tracing::warn!(
                "Failed to create exec workspace {:?}: {}. Falling back to {:?}",
                workspace_path,
                e,
                fallback
            );
            return resolve_to_absolute(fallback.to_path_buf());
        }
        tracing::info!("Created exec workspace: {:?}", workspace_path);
    }

    workspace_path
}

/// Configuration for building a [`ToolRegistry`].
///
/// Only dynamic / mode-specific dependencies are kept as fields.
/// Infra singletons (config, database) are fetched from globals inside
/// [`build_tool_registry`].
pub struct ToolRegistryConfig {
    /// Optional subagent spawner for the spawn tools.
    pub subagent_spawner: Option<Arc<dyn SubagentSpawner>>,
    /// Extra tools to register (e.g. gateway-specific `MessageTool`, `CronTool`).
    pub extra_tools: Vec<(Box<dyn Tool>, ToolMetadata)>,
    /// Optional LLM provider for external tool discovery.
    pub provider: Option<Arc<dyn gasket_providers::LlmProvider>>,
    /// Model identifier for plan-generation tools.
    pub model: Option<String>,
    /// Optional semantic history search (embedding feature).
    #[cfg(feature = "embedding")]
    pub history_search: Option<HistorySearchParams>,
    /// Determines whether spawn tools (`spawn`, `spawn_parallel`) are registered.
    /// Worker contexts must use `AgentRole::Worker` to omit them.
    pub role: gasket_types::AgentRole,
    /// Shared JSON store — the sole storage backend.
    pub store: Arc<gasket_storage::JsonStore>,
}

/// Parameters needed to construct the `history_search` tool.
#[cfg(feature = "embedding")]
#[derive(Clone)]
pub struct HistorySearchParams {
    pub searcher: std::sync::Arc<gasket_embedding::RecallSearcher>,
    pub config: gasket_embedding::RecallConfig,
}

/// Build a [`ToolRegistry`] with common tools shared across all modes.
///
/// This function registers all common tools (filesystem, web, memory, etc.) and
/// accepts extra tools via the `extra_tools` field for mode-specific additions.
/// Infra singletons (config, database) are read from globals — they must be
/// initialized by the caller before invoking this function.
pub fn build_tool_registry(registry_config: ToolRegistryConfig) -> ToolRegistry {
    let ToolRegistryConfig {
        subagent_spawner,
        extra_tools,
        provider,
        #[cfg(feature = "embedding")]
        history_search,
        role,
        store,
        ..
    } = registry_config;

    let config = crate::config::get_config();
    let workspace = resolve_exec_workspace(config, std::path::Path::new("."));

    let mut tools = ToolRegistry::new();

    // ── Core tools (filesystem, web, exec, spawn) ─────────────
    CoreToolProvider::new(config, &workspace, subagent_spawner.clone(), role)
        .register_tools(&mut tools);

    // ── System tools (history query, session management) ──────
    let event_store: Arc<dyn gasket_storage::EventStoreTrait> = store.clone();
    let session_store: Arc<dyn gasket_storage::SessionStoreTrait> = store.clone();
    SystemToolProvider::new(Some(event_store), Some(session_store)).register_tools(&mut tools);

    // Extra tools (e.g. gateway-specific MessageTool, CronTool)
    for (tool, metadata) in extra_tools {
        tools.register_with_metadata(tool, metadata);
    }

    // ── Embedding-based history search (conditional) ───────────
    #[cfg(feature = "embedding")]
    {
        use super::HistorySearchTool;
        if let Some(params) = history_search {
            tools.register(Box::new(HistorySearchTool::new(
                params.searcher,
                params.config,
            )));
        }
    }

    // Discover native workflows — only when subagent spawning is available.
    // Skill-mode workflows are injected as skills via the skills system;
    // only tool-mode workflows are registered as callable tools.
    if role.can_spawn() {
        let workflows_dir = workspace.join("workflows");
        tracing::info!("Looking for native workflows in {:?}", workflows_dir);
        match super::discover_workflows(workflows_dir.as_path()) {
            Ok(workflow_tools) => {
                for tool in workflow_tools {
                    tools.register(Box::new(tool));
                }
            }
            Err(e) => {
                tracing::warn!("Failed to discover workflows: {}", e);
            }
        }
    }

    // Discover external plugins — engine resources are injected at construction time.
    let engine_resources = provider.map(|p| {
        let tools_arc = Arc::new(tools.clone());
        crate::external_tools::EngineResources {
            tool_registry: tools_arc,
            provider: p,
        }
    });
    if let Err(e) = crate::external_tools::discover_plugins(&mut tools, engine_resources) {
        tracing::warn!("Failed to discover script tools: {}", e);
    }

    tools
}
