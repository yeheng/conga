//! Sandbox configuration types
//!
//! Configuration for sandbox backends, approval system, and audit logging.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{CommandPolicyConfig, ResourceLimits};

/// Main sandbox configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Enable sandbox (default: false, opt-in)
    #[serde(default)]
    pub enabled: bool,

    /// Sandbox backend: auto | fallback | bwrap | sandbox-exec | docker
    #[serde(default = "default_backend")]
    pub backend: String,

    /// Size of /tmp tmpfs inside sandbox in MB (default: 64)
    #[serde(default = "default_tmp_size_mb")]
    pub tmp_size_mb: u32,

    /// Workspace directory for sandboxed operations
    #[serde(default)]
    pub workspace: Option<PathBuf>,

    /// Resource limits
    #[serde(default)]
    pub limits: ResourceLimits,

    /// Command policy
    #[serde(default)]
    pub policy: CommandPolicyConfig,

    /// Audit configuration
    #[serde(default)]
    pub audit: AuditConfig,
}

fn default_backend() -> String {
    "auto".to_string()
}

fn default_tmp_size_mb() -> u32 {
    64
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_backend(),
            tmp_size_mb: default_tmp_size_mb(),
            workspace: None,
            limits: ResourceLimits::default(),
            policy: CommandPolicyConfig::default(),
            audit: AuditConfig::default(),
        }
    }
}

impl SandboxConfig {
    /// Create a new configuration with sandbox enabled
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }

    /// Create a fallback (no sandbox) configuration
    pub fn fallback() -> Self {
        Self {
            enabled: false,
            backend: "fallback".to_string(),
            ..Default::default()
        }
    }

    /// Set the workspace directory
    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// Set the backend
    pub fn with_backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = backend.into();
        self
    }
}

/// Audit logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Enable audit logging
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,

    /// Path to audit log file
    #[serde(default)]
    pub log_file: Option<PathBuf>,

    /// Maximum log file size in MB
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,

    /// Whether to include command output in logs
    #[serde(default)]
    pub log_output: bool,

    /// Whether to include environment variables in logs
    #[serde(default)]
    pub log_env: bool,
}

fn default_audit_enabled() -> bool {
    true
}

fn default_max_size_mb() -> u64 {
    100
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            log_file: None,
            max_size_mb: default_max_size_mb(),
            log_output: false,
            log_env: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SandboxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.backend, "auto");
        assert_eq!(config.tmp_size_mb, 64);
    }

    #[test]
    fn test_enabled_config() {
        let config = SandboxConfig::enabled();
        assert!(config.enabled);
    }

    #[test]
    fn test_fallback_config() {
        let config = SandboxConfig::fallback();
        assert!(!config.enabled);
        assert_eq!(config.backend, "fallback");
    }

    #[test]
    fn test_builder_pattern() {
        let config = SandboxConfig::enabled()
            .with_workspace("/tmp/workspace")
            .with_backend("bwrap");
        assert!(config.enabled);
        assert_eq!(config.workspace, Some(PathBuf::from("/tmp/workspace")));
        assert_eq!(config.backend, "bwrap");
    }

    #[test]
    fn test_deserialize_full_config() {
        let yaml = r#"
enabled: true
backend: bwrap
tmp_size_mb: 128
workspace: /home/user/.gasket
limits:
  max_memory_mb: 1024
  max_cpu_secs: 30
policy:
  allowlist:
    - ls
    - cat
  denylist:
    - "rm -rf /"
audit:
  enabled: true
  log_file: /var/log/gasket/audit.log
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.backend, "bwrap");
        assert_eq!(config.tmp_size_mb, 128);
        assert_eq!(config.limits.max_memory_mb, 1024);
        assert_eq!(config.policy.allowlist, vec!["ls", "cat"]);
        assert!(config.audit.enabled);
    }
}
