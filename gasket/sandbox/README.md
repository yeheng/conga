# gasket-sandbox

Secure sandbox execution module for gasket with multi-platform support and audit logging.

> **Approval:** runtime tool-call approval is handled by the live
> `gasket_types::ApprovalCallback` path (wired through the tool registry), not
> by this crate.

## Features

- **Multi-platform support**: Linux (bwrap), macOS (sandbox-exec), Windows (Job Objects)
- **Audit logging**: Comprehensive logging of all operations
- **Resource limits**: Memory, CPU time, output size, and process count limits
- **Command policy**: Allowlist/denylist for command filtering

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
gasket-sandbox = { path = "gasket-sandbox" }
```

## Feature Flags

- `default` - Includes `platform-native` and `audit` features
- `platform-native` - Platform-native sandbox (bwrap, sandbox-exec, Job Objects)
- `audit` - Audit logging
- `full` - All features

## Quick Start

```rust
use gasket_sandbox::{ProcessManager, SandboxConfig};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a fallback (no sandbox) configuration
    let config = SandboxConfig::fallback();

    // Create a process manager
    let manager = ProcessManager::new(config);

    // Execute a command
    let result = manager.execute("echo hello", Path::new("/tmp")).await?;

    println!("Output: {}", result.stdout);
    Ok(())
}
```

## Configuration

```yaml
sandbox:
  # Enable sandbox
  enabled: true

  # Backend: auto | fallback | bwrap | sandbox-exec | docker
  backend: auto

  # Resource limits
  limits:
    max_memory_mb: 512
    max_cpu_secs: 60
    max_output_bytes: 1048576
    max_processes: 10

  # Command policy
  policy:
    allowlist: []
    denylist:
      - "rm -rf /"
      - "mkfs"

  # Audit logging
  audit:
    enabled: true
    log_file: ~/.gasket/audit.log
```

## Platform Support

| Platform | Backend | Description |
|----------|---------|-------------|
| Linux | bwrap | Bubblewrap namespace isolation |
| macOS | sandbox-exec | Apple Seatbelt sandbox |
| Windows | Job Objects | Windows Job Objects limits |
| All | fallback | Direct execution with ulimit |

## Audit Logging

```rust
use gasket_sandbox::audit::{AuditLog, AuditEvent, AuditConfig};

let config = AuditConfig::default();
let log = AuditLog::new(&config)?;
log.initialize().await?;

// Log an event
let event = AuditEvent::command_start("ls -la", "/home/user");
log.write(&event).await?;
```

## License

MIT
