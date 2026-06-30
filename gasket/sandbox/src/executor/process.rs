//! Process manager for command execution

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::instrument;

use super::ExecutionResult;
use crate::backend::{create_backend, IsolationLevel, SandboxBackend};
use crate::config::{CommandPolicy, PolicyVerdict, SandboxConfig};
use crate::error::{Result, SandboxError};
#[cfg(feature = "audit")]
use crate::audit::AuditLog;

/// Process manager for executing commands
pub struct ProcessManager {
    config: SandboxConfig,
    policy: CommandPolicy,
    backend: Box<dyn SandboxBackend>,
    timeout: Duration,
    #[cfg(feature = "audit")]
    audit: Option<Arc<AuditLog>>,
}

impl ProcessManager {
    /// Create a new process manager.
    ///
    /// Returns `Err(SandboxError::ConfigError)` if `config.enabled = true` but
    /// the requested backend is not available on this platform. This is the
    /// fail-closed behavior: never silently degrade to an unsandboxed
    /// executor behind the caller's back. Callers that explicitly want
    /// unsandboxed execution must set `config.enabled = false`.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        let backend = create_backend(&config)?;
        let policy = CommandPolicy::from_config(&config.policy);
        let timeout = Duration::from_secs(120);

        Ok(Self {
            config,
            policy,
            backend,
            timeout,
            #[cfg(feature = "audit")]
            audit: None,
        })
    }

    /// Create with custom timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set an audit log for recording command executions.
    #[cfg(feature = "audit")]
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Execute a command, applying the manager's configured wall-clock timeout.
    #[instrument(name = "process.execute", skip(self))]
    pub async fn execute(&self, command: &str, working_dir: &Path) -> Result<ExecutionResult> {
        self.execute_with_timeout(command, working_dir, self.timeout)
            .await
    }

    /// Execute a command with a caller-supplied wall-clock timeout.
    pub async fn execute_with_timeout(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<ExecutionResult> {
        self.execute_inner(command, working_dir, timeout).await
    }

    async fn execute_inner(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<ExecutionResult> {
        // Step 1: Policy check
        if let PolicyVerdict::Deny(reason) = self.policy.check(command) {
            return Err(SandboxError::PolicyDenied(reason));
        }

        // Step 2: Audit — command start
        #[cfg(feature = "audit")]
        {
            if let Some(ref audit) = self.audit {
                if let Err(e) = audit.log_command(command, working_dir, None).await {
                    tracing::warn!("Audit log command start failed: {}", e);
                }
            }
        }

        // Step 3: Execute
        let start = Instant::now();
        let result = self
            .backend
            .execute_with_timeout(command, working_dir, &self.config, timeout)
            .await?;
        let duration = start.elapsed();

        let exec_result = ExecutionResult {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            timed_out: result.timed_out,
            resource_exceeded: result.resource_exceeded,
            duration_ms: duration.as_millis() as u64,
        };

        // Step 4: Audit — command end
        #[cfg(feature = "audit")]
        {
            if let Some(ref audit) = self.audit {
                if let Err(e) = audit
                    .log_command_end(
                        command,
                        exec_result.exit_code,
                        exec_result.duration_ms,
                        exec_result.timed_out,
                        None,
                    )
                    .await
                {
                    tracing::warn!("Audit log command end failed: {}", e);
                }
            }
        }

        Ok(exec_result)
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    /// Effective isolation level provided by the active backend.
    ///
    /// Prefer this over `is_sandboxed()` — "sandboxed" is a yes/no whose
    /// answer is misleading on Windows (resource limits only) and macOS
    /// (deprecated Seatbelt).
    pub fn isolation_level(&self) -> IsolationLevel {
        self.backend.isolation_level()
    }

    /// Returns `true` if the user enabled sandboxing AND the backend
    /// actually provides some form of isolation. Kept for backward
    /// compatibility with existing callers; new code should use
    /// [`isolation_level`](Self::isolation_level) for a precise answer.
    pub fn is_sandboxed(&self) -> bool {
        self.config.enabled && self.backend.isolation_level() != IsolationLevel::None
    }

    pub fn provides_filesystem_isolation(&self) -> bool {
        self.backend.provides_filesystem_isolation()
    }

    pub fn policy(&self) -> &CommandPolicy {
        &self.policy
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_manager_fallback() {
        let config = SandboxConfig::fallback();
        let manager = ProcessManager::new(config).expect("fallback always available");
        assert_eq!(manager.backend_name(), "fallback");
        assert!(!manager.is_sandboxed());
        assert_eq!(manager.isolation_level(), IsolationLevel::None);
    }

    #[tokio::test]
    async fn test_execute_simple_command() {
        let config = SandboxConfig::fallback();
        let manager = ProcessManager::new(config).expect("fallback always available");
        let result = manager.execute("echo hello", Path::new("/tmp")).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_success());
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_with_timeout() {
        let config = SandboxConfig::fallback();
        let manager = ProcessManager::new(config).expect("fallback always available");
        let result = manager
            .execute_with_timeout("sleep 10", Path::new("/tmp"), Duration::from_millis(100))
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SandboxError::Timeout { .. }));
    }

    /// Regression: requesting a backend not available on the current platform
    /// must FAIL CLOSED — never silently fall back to the unsandboxed executor.
    #[tokio::test]
    async fn test_unknown_backend_is_hard_error() {
        let mut config = SandboxConfig::fallback();
        config.enabled = true;
        config.backend = "no-such-backend".into();
        let result = ProcessManager::new(config);
        assert!(result.is_err(), "unknown backend must error");
        match result {
            Err(SandboxError::ConfigError(_)) => {}
            Err(other) => panic!("expected ConfigError, got {:?}", other),
            Ok(_) => unreachable!("checked is_err above"),
        }
    }
}
