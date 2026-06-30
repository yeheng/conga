//! Subagent management - simplified pure function approach
//!
//! This module provides lightweight subagent spawning using a functional API
//! instead of the previous Java-style Manager + Builder pattern.
//!
//! ## Quick Start
//!
//! ```ignore
//! use gasket_engine::subagents::{spawn_subagent, TaskSpec, SimpleSpawner};
//! use tokio_util::sync::CancellationToken;
//!
//! // Simple function-based spawning
//! let handle = spawn_subagent(
//!     provider,
//!     tools,
//!     workspace,
//!     TaskSpec::new("uuid-123", "Analyze this code"),
//!     Some(event_tx),
//!     result_tx,
//!     None,
//!     CancellationToken::new(),
//!     None,
//! );
//!
//! // Or use the SimpleSpawner for trait-based spawning (preferred)
//! let spawner = SimpleSpawner::new(provider, tools, workspace);
//! let result = spawner.spawn("task".to_string(), None).await?;
//! ```

pub mod manager;
pub mod runner;
pub mod tracker;

// Re-exports - the new functional API
pub use manager::{spawn_subagent, SimpleSpawner, TaskSpec};

// Re-exports - tracker types
pub use tracker::{SubagentResult, SubagentTracker, TrackerError};

// Re-exports - runner
pub use runner::{run_subagent, ModelResolver};
