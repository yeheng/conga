//! Plugin manifest types
//!
//! This module defines the YAML manifest format for external plugins.
//! Plugins are declared via YAML manifests that describe their runtime
//! configuration, protocol, parameters, and required permissions.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Manifest describing an external plugin.
///
/// The manifest is loaded from a YAML file and defines how the script
/// should be executed, what protocol it uses, what permissions it needs,
/// and what parameters it accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Tool name (must be unique across all tools)
    pub name: String,

    /// Human-readable description of what the tool does
    pub description: String,

    /// Version string (default: "")
    #[serde(default)]
    pub version: String,

    /// Runtime configuration (command, args, working directory, etc.)
    pub runtime: RuntimeConfig,

    /// Communication protocol (default: Simple)
    #[serde(default)]
    pub protocol: PluginProtocol,

    /// JSON Schema defining the tool's parameters
    pub parameters: serde_json::Value,

    /// Required permissions (default: empty = deny all)
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

/// Communication protocol for plugins.
///
/// - Simple: stdin/stdout with newline-delimited JSON
/// - JsonRpc: JSON-RPC 2.0 over stdio
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginProtocol {
    /// Simple stdin/stdout protocol (default)
    ///
    /// Input: JSON object via stdin
    /// Output: JSON object via stdout
    /// Errors: Non-zero exit code or stderr output
    #[default]
    Simple,

    /// JSON-RPC 2.0 over stdio
    ///
    /// Full JSON-RPC 2.0 protocol with method calls, notifications,
    /// and structured error responses.
    JsonRpc,
}

/// Runtime configuration for script execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Command to execute (e.g., "python", "node", "/path/to/script")
    pub command: String,

    /// Command arguments (default: [])
    #[serde(default)]
    pub args: Vec<String>,

    /// Working directory relative to manifest (default: ".")
    #[serde(default = "default_working_dir")]
    pub working_dir: String,

    /// Per-call timeout in seconds.
    ///
    /// Bounds the wait time for a single tool invocation (Simple mode: total
    /// process runtime; JSON-RPC mode: one `call()` round-trip). When omitted,
    /// the global `plugin_timeout_secs` from agent config is used.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Idle timeout in seconds for JSON-RPC daemon processes.
    ///
    /// After this much idle time the daemon is considered expired and a fresh
    /// process is spawned on next call. Ignored in Simple mode. When omitted,
    /// defaults to `4 × call_timeout` (i.e. keep the daemon alive across a few
    /// idle calls but not forever).
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,

    /// Environment variables to pass to the script (default: {})
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_working_dir() -> String {
    ".".to_string()
}

/// Permission grants access to specific Gasket capabilities.
///
/// Permissions map to JSON-RPC method names that will be made available
/// to the plugin. The default-deny policy means omitted permissions
/// result in no access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Permission to call LLM chat API
    LlmChat,

    /// Permission to spawn subagents
    SubagentSpawn,

    /// Permission to send messages to channels
    MessageSend,

    /// Permission to ask the user a question and wait for their reply
    UserAsk,
}

impl Permission {
    /// Get the JSON-RPC method name for this permission.
    ///
    /// Each permission maps to a specific RPC method that will be
    /// made available to the plugin.
    pub fn method_name(&self) -> &'static str {
        match self {
            Permission::LlmChat => "llm/chat",
            Permission::SubagentSpawn => "subagent/spawn",
            Permission::MessageSend => "message/send",
            Permission::UserAsk => "user/ask",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_manifest() {
        let yaml = r#"
name: "example_tool"
description: "An example tool"
version: "1.0.0"
runtime:
  command: "python"
  args: ["script.py"]
parameters:
  type: "object"
  properties:
    input:
      type: "string"
  required: ["input"]
"#;

        let manifest: PluginManifest =
            serde_yaml::from_str(yaml).expect("Failed to parse manifest");

        assert_eq!(manifest.name, "example_tool");
        assert_eq!(manifest.description, "An example tool");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.runtime.command, "python");
        assert_eq!(manifest.runtime.args, vec!["script.py"]);
        assert_eq!(manifest.runtime.working_dir, ".");
        assert_eq!(manifest.runtime.timeout_secs, None);
        assert_eq!(manifest.protocol, PluginProtocol::Simple);
        assert!(manifest.runtime.env.is_empty());
        assert!(manifest.permissions.is_empty());
    }

    #[test]
    fn test_parse_jsonrpc_manifest_with_permissions() {
        let yaml = r#"
name: "advanced_tool"
description: "A tool with full configuration"
version: "2.0.0"
protocol: json_rpc
runtime:
  command: "node"
  args: ["index.js", "--verbose"]
  working_dir: "./scripts"
  timeout_secs: 300
  env:
    NODE_ENV: "production"
    API_KEY: "test-key"
parameters:
  type: "object"
  properties:
    query:
      type: "string"
  required: ["query"]
permissions:
  - llm_chat
  - subagent_spawn
  - message_send
"#;

        let manifest: PluginManifest =
            serde_yaml::from_str(yaml).expect("Failed to parse manifest");

        assert_eq!(manifest.name, "advanced_tool");
        assert_eq!(manifest.version, "2.0.0");
        assert_eq!(manifest.protocol, PluginProtocol::JsonRpc);
        assert_eq!(manifest.runtime.command, "node");
        assert_eq!(manifest.runtime.args, vec!["index.js", "--verbose"]);
        assert_eq!(manifest.runtime.working_dir, "./scripts");
        assert_eq!(manifest.runtime.timeout_secs, Some(300));
        assert_eq!(manifest.runtime.env.len(), 2);
        assert_eq!(
            manifest.runtime.env.get("NODE_ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(
            manifest.runtime.env.get("API_KEY"),
            Some(&"test-key".to_string())
        );
        assert_eq!(manifest.permissions.len(), 3);
        assert!(manifest.permissions.contains(&Permission::LlmChat));
        assert!(manifest.permissions.contains(&Permission::SubagentSpawn));
        assert!(manifest.permissions.contains(&Permission::MessageSend));
    }

    #[test]
    fn test_default_deny_no_permissions() {
        let yaml = r#"
name: "restricted_tool"
description: "A tool with no permissions"
runtime:
  command: "bash"
  args: ["script.sh"]
parameters:
  type: "object"
  properties: {}
"#;

        let manifest: PluginManifest =
            serde_yaml::from_str(yaml).expect("Failed to parse manifest");

        // Verify defaults for omitted fields
        assert_eq!(manifest.version, "");
        assert_eq!(manifest.protocol, PluginProtocol::Simple);
        assert_eq!(manifest.runtime.working_dir, ".");
        assert_eq!(manifest.runtime.timeout_secs, None);
        assert!(manifest.runtime.env.is_empty());

        // Verify default-deny: no permissions specified = empty vector
        assert!(manifest.permissions.is_empty());
    }

    #[test]
    fn test_permission_serde_roundtrip() {
        let permissions = vec![Permission::LlmChat, Permission::SubagentSpawn];

        // Test serialization
        let yaml = serde_yaml::to_string(&permissions).expect("Failed to serialize");
        let parsed: Vec<Permission> = serde_yaml::from_str(&yaml).expect("Failed to deserialize");

        // Verify all permissions survived roundtrip
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&Permission::LlmChat));
        assert!(parsed.contains(&Permission::SubagentSpawn));
    }

    #[test]
    fn test_permission_method_names() {
        assert_eq!(Permission::LlmChat.method_name(), "llm/chat");
        assert_eq!(Permission::SubagentSpawn.method_name(), "subagent/spawn");
        assert_eq!(Permission::UserAsk.method_name(), "user/ask");
    }

    #[test]
    fn test_permission_user_ask_serde_roundtrip() {
        let perms = vec![Permission::UserAsk];
        let yaml = serde_yaml::to_string(&perms).expect("serialize");
        assert!(yaml.contains("user_ask"));
        let parsed: Vec<Permission> = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed, vec![Permission::UserAsk]);
    }

    #[test]
    fn manifest_rejects_unknown_top_level_field() {
        let yaml = r#"
name: "x"
description: "y"
runtime:
  command: "cat"
parameters:
  type: object
  properties: {}
unknownfield: "leftover"
"#;
        let result: Result<PluginManifest, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown top-level field should be rejected"
        );
    }

    #[test]
    fn manifest_rejects_unknown_runtime_field() {
        let yaml = r#"
name: "x"
description: "y"
runtime:
  command: "cat"
  time_secs: 30   # typo for timeout_secs
parameters:
  type: object
  properties: {}
"#;
        let result: Result<PluginManifest, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "typo in runtime field should be rejected (not silently dropped)"
        );
    }
}
