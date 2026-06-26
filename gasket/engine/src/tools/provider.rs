//! Tool providers — decouple tool registration from `build_tool_registry`.
//!
//! Each subsystem (filesystem, system) implements `ToolProvider` and
//! registers its own tools. `build_tool_registry` only orchestrates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::SubagentSpawner;
use gasket_storage::{EventStoreTrait, SessionStoreTrait};

use super::{
    registry::ToolRegistry, AskUserTool, ClearSessionTool, EditFileTool, ExecTool, HistoryQueryTool,
    ListDirTool, NewSessionTool, ReadFileTool, SpawnParallelTool, SpawnTool, ToolMetadata,
    WebFetchTool, WebSearchTool, WriteFileTool,
};

/// Trait for subsystems that provide tools to the registry.
pub trait ToolProvider: Send + Sync {
    /// Register this provider's tools into the given registry.
    fn register_tools(&self, registry: &mut ToolRegistry);
}

/// Register a tool with full metadata — used by all ToolProvider impls.
macro_rules! reg {
    ($registry:expr, $tool:expr, $display:literal, $cat:literal, [$($tag:literal),*], $approval:literal, $mutating:literal) => {
        $registry.register_with_metadata(
            Box::new($tool),
            ToolMetadata {
                display_name: $display.to_string(),
                category: $cat.to_string(),
                tags: vec![$($tag.to_string()),*],
                requires_approval: $approval,
                is_mutating: $mutating,
            },
        );
    };
}

// ---------------------------------------------------------------------------
// CoreToolProvider — filesystem, web, exec, spawn
// ---------------------------------------------------------------------------

/// Provides core tools that are always available.
pub struct CoreToolProvider {
    restrict: bool,
    allowed_dir: Option<PathBuf>,
    exec_workspace: PathBuf,
    web_config: crate::config::WebToolsConfig,
    exec_config: crate::config::ExecToolConfig,
    _subagent_spawner: Option<Arc<dyn SubagentSpawner>>,
    role: gasket_types::AgentRole,
}

impl CoreToolProvider {
    pub fn new(
        config: &Config,
        workspace: &Path,
        subagent_spawner: Option<Arc<dyn SubagentSpawner>>,
        role: gasket_types::AgentRole,
    ) -> Self {
        let restrict = config.tools.restrict_to_workspace;
        let allowed_dir = if restrict {
            Some(workspace.to_path_buf())
        } else {
            None
        };
        let exec_workspace = super::builder::resolve_exec_workspace(config, workspace);
        Self {
            restrict,
            allowed_dir,
            exec_workspace,
            web_config: config.tools.web.clone(),
            exec_config: config.tools.exec.clone(),
            _subagent_spawner: subagent_spawner,
            role,
        }
    }
}

impl ToolProvider for CoreToolProvider {
    fn register_tools(&self, registry: &mut ToolRegistry) {
        // Safe read-only tools
        reg!(
            registry,
            ReadFileTool::new(self.allowed_dir.clone()),
            "Read File",
            "filesystem",
            ["read", "file"],
            false,
            false
        );
        reg!(
            registry,
            ListDirTool::new(self.allowed_dir.clone()),
            "List Directory",
            "filesystem",
            ["read", "directory"],
            false,
            false
        );
        reg!(
            registry,
            WebFetchTool::with_config(Some(self.web_config.clone())).unwrap_or_else(|e| {
                tracing::warn!(
                    "Failed to create WebFetchTool with proxy config: {}. Using default.",
                    e
                );
                WebFetchTool::new()
            }),
            "Web Fetch",
            "web",
            ["http", "fetch"],
            false,
            false
        );
        reg!(
            registry,
            WebSearchTool::new(Some(self.web_config.clone())),
            "Web Search",
            "web",
            ["search", "web"],
            false,
            false
        );

        // User interaction
        reg!(
            registry,
            AskUserTool::new(),
            "Ask User",
            "interaction",
            ["user", "prompt"],
            false,
            false
        );

        // Dangerous mutating tools
        reg!(
            registry,
            WriteFileTool::new(self.allowed_dir.clone()),
            "Write File",
            "filesystem",
            ["write", "file"],
            true,
            true
        );
        reg!(
            registry,
            EditFileTool::new(self.allowed_dir.clone()),
            "Edit File",
            "filesystem",
            ["edit", "file"],
            true,
            true
        );
        reg!(
            registry,
            ExecTool::from_config(
                self.exec_workspace.clone(),
                &self.exec_config,
                self.restrict
            ),
            "Execute Command",
            "system",
            ["shell", "exec"],
            true,
            true
        );

        // Spawn tools — only the Orchestrator gets these; Workers see neither.
        if self.role.can_spawn() {
            reg!(
                registry,
                SpawnTool::new(),
                "Spawn Subagent",
                "system",
                ["spawn", "agent"],
                false,
                false
            );
            reg!(
                registry,
                SpawnParallelTool::new(),
                "Spawn Parallel",
                "system",
                ["spawn", "parallel", "agent"],
                false,
                false
            );
        }
    }
}

// ---------------------------------------------------------------------------
// SystemToolProvider — history query, session management
// ---------------------------------------------------------------------------

/// Provides system tools backed by the event/session stores.
pub struct SystemToolProvider {
    event_store: Option<Arc<dyn EventStoreTrait>>,
    session_store: Option<Arc<dyn SessionStoreTrait>>,
}

impl SystemToolProvider {
    pub fn new(
        event_store: Option<Arc<dyn EventStoreTrait>>,
        session_store: Option<Arc<dyn SessionStoreTrait>>,
    ) -> Self {
        Self {
            event_store,
            session_store,
        }
    }
}

impl ToolProvider for SystemToolProvider {
    fn register_tools(&self, registry: &mut ToolRegistry) {
        if let (Some(es), Some(ss)) = (self.event_store.clone(), self.session_store.clone()) {
            reg!(
                registry,
                HistoryQueryTool::new(es.clone()),
                "Query History",
                "history",
                ["history", "search"],
                false,
                false
            );
            reg!(
                registry,
                ClearSessionTool::new(es.clone(), ss.clone()),
                "Clear Session History",
                "system",
                ["session", "cleanup", "history"],
                true,
                true
            );
            reg!(
                registry,
                NewSessionTool::new(es, ss),
                "New Session",
                "system",
                ["session", "new", "reset"],
                true,
                true
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use gasket_types::AgentRole;

    #[test]
    fn worker_provider_does_not_register_spawn_tools() {
        let cfg = crate::config::Config::default();
        let mut registry = ToolRegistry::new();
        CoreToolProvider::new(&cfg, std::path::Path::new("/tmp"), None, AgentRole::Worker)
            .register_tools(&mut registry);
        assert!(
            registry.get("spawn").is_none(),
            "Worker registry must not contain `spawn`"
        );
        assert!(
            registry.get("spawn_parallel").is_none(),
            "Worker registry must not contain `spawn_parallel`"
        );
    }

    #[test]
    fn orchestrator_provider_registers_spawn_tools() {
        let cfg = crate::config::Config::default();
        let mut registry = ToolRegistry::new();
        CoreToolProvider::new(
            &cfg,
            std::path::Path::new("/tmp"),
            None,
            AgentRole::Orchestrator,
        )
        .register_tools(&mut registry);
        assert!(
            registry.get("spawn").is_some(),
            "Orchestrator registry must contain `spawn`"
        );
        assert!(
            registry.get("spawn_parallel").is_some(),
            "Orchestrator registry must contain `spawn_parallel`"
        );
    }
}
