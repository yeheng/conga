//! # gasket-sandbox
//!
//! Secure sandbox execution module for gasket with multi-platform support and
//! audit logging.
//!
//! ## Features
//!
//! - **Multi-platform support**: Linux (bwrap), macOS (sandbox-exec), Windows (Job Objects)
//! - **Audit logging**: Comprehensive logging of all operations
//! - **Resource limits**: Memory, CPU time, output size, and process count limits
//! - **Command policy**: Allowlist/denylist for command filtering
//!
//! ## Approval
//!
//! Runtime tool-call approval is handled by the live
//! `gasket_types::ApprovalCallback` path (wired through the tool registry),
//! not by this crate.
//!
//! ## Quick Start
//!
//! ```rust
//! use gasket_sandbox::{ProcessManager, SandboxConfig};
//! use std::path::Path;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create a fallback (no sandbox) configuration
//!     let config = SandboxConfig::fallback();
//!
//!     // Create a process manager (fails if no isolation backend can be built).
//!     let manager = ProcessManager::new(config)?;
//!
//!     // Execute a command
//!     let result = manager.execute("echo hello", Path::new("/tmp")).await?;
//!
//!     println!("Output: {}", result.stdout);
//!     Ok(())
//! }
//! ```

pub mod backend;
pub mod config;
pub mod error;
pub mod executor;

#[cfg(feature = "audit")]
pub mod audit;

// Re-exports for convenience
pub use backend::{available_backends, create_backend, Platform, SandboxBackend};
pub use config::{AuditConfig, CommandPolicy, CommandPolicyConfig, ResourceLimits, SandboxConfig};
pub use error::{Result, SandboxError};
pub use executor::{ExecutionResult, ProcessManager};

#[cfg(feature = "audit")]
pub use audit::{AuditEvent, AuditEventType, AuditLog};

/// Prelude module for common imports
pub mod prelude {
    pub use crate::backend::{Platform, SandboxBackend};
    pub use crate::config::SandboxConfig;
    pub use crate::error::{Result, SandboxError};
    pub use crate::executor::{ExecutionResult, ProcessManager};

    #[cfg(feature = "audit")]
    pub use crate::audit::{AuditEvent, AuditLog};
}

/// Get the version of the sandbox module
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Get the current platform
pub fn current_platform() -> Platform {
    Platform::current()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!version().is_empty());
    }

    #[test]
    fn test_current_platform() {
        let platform = current_platform();
        // Just verify it doesn't panic
        let _ = platform.as_str();
    }

    #[test]
    fn test_available_backends() {
        let backends = available_backends();
        assert!(backends.contains(&"fallback"));
    }
}
