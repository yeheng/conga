//! conga — a pi-style pluggable agent core.
//!
//! A single `agent_loop` function plus an `ExtensionApi` trait. Extra tools and
//! hooks come from in-process Rust extension crates that call `register` at
//! host startup (optionally behind Cargo features). No dynamic library loading.

pub mod agent_loop;
pub mod cancel;
pub mod config_file;
pub mod error;
pub mod extension;
pub mod guard;
pub mod providers;
pub mod proxy;
pub mod steer;
pub mod storage;
pub mod types;

#[cfg(test)]
pub(crate) mod test_util;
pub use agent_loop::{agent_loop, run_agent_loop};
pub use cancel::CancelSignal;
pub use error::{AgentError, ToolError};
pub use extension::{ExtensionApi, ExtensionApiImpl};
pub use providers::{AnthropicProvider, ConfigError, OpenAiCompat, ProviderConfig};
pub use steer::SteerQueue;

pub use storage::{is_valid_session_id, EventStorage, JsonlStorage, SessionMeta};

pub use types::context::{
    AgentContext, AgentLoopConfig, AgentTunables, ModelSpec, ProviderApi, RetryPolicy, StreamChunk,
    StreamFn,
};
pub use types::event::{AgentEvent, ContentDelta};
pub use types::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolResultMessage, Usage, UserMessage,
};
pub use types::session_event::{
    billing_scope_start, cache_stats, derive_messages, repair_unanswered_tool_calls, row_view,
    row_view_from_json, view_scope_start, visible_count_from_rows, visible_message_count,
    CacheStats, CancelCause, RowView, SessionEvent, TurnEndReason,
};
pub use types::tool::{
    HookChain, RiskLevel, ToolCallCtx, ToolCallVerdict, ToolContext, ToolDefinition, ToolFn,
    ToolResult,
};

/// Current monotonically-increasing time in milliseconds since UNIX epoch.
///
/// Used for message timestamps.
pub fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::now;

    /// `now` is an epoch-millisecond clock: far past 2020 and never running
    /// backwards between two reads.
    #[test]
    fn now_is_epoch_millis_and_non_decreasing() {
        let first = now();
        let second = now();
        // 2020-09-13T12:26:40Z - a sane floor for "milliseconds since epoch".
        assert!(
            first > 1_600_000_000_000,
            "now() must be epoch milliseconds, got {first}"
        );
        assert!(second >= first, "clock went backwards: {first} -> {second}");
    }
}
