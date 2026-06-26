//! Agent pipeline lifecycle hooks
//!
//! Provides a unified hook mechanism for the agent pipeline:
//! - `HookPoint`: Execution points in the pipeline
//! - `HookContext`: Context passed to hooks
//! - `PipelineHook`: Trait for implementing hooks
//! - `HookRegistry`: Registry for managing hooks
//!
//! ## Hook Points
//!
//! Hooks can be attached at five execution points:
//! - `BeforeRequest`: Before the request is processed (can modify input)
//! - `AfterHistory`: After history is loaded (can add context)
//! - `BeforeLLM`: Before sending to LLM (last chance to modify)
//! - `AfterToolCall`: After a tool call completes (logging/auditing)
//! - `AfterResponse`: After the response is generated (notifications)
//!
//! ## Execution Strategies
//!
//! - **Sequential** (BeforeRequest, AfterHistory, BeforeLLM):
//!   Hooks run one after another, can modify messages, can abort.
//! - **Parallel** (AfterToolCall, AfterResponse):
//!   Hooks run concurrently with readonly access, fire-and-forget.

mod external;
mod registry;
mod types;
mod vault;

pub use external::{ExternalHookInput, ExternalHookOutput, ExternalHookRunner, ExternalShellHook};
pub use registry::{HookBuilder, HookRegistry, PipelineHook};
pub use types::{
    ExecutionStrategy, HookAction, HookContext, HookPoint, MutableContext, ReadonlyContext,
    ToolCallInfo,
};
pub use vault::VaultHook;
