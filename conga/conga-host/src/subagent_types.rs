//! Subagent orchestration types: event protocol + spawner trait.
//!
//! The `spawn_subagents` tool captures a `SubagentSpawner` in its execute
//! closure at construction. It calls the spawner to fan out parallel
//! sub-agent loops.
//! Events are emitted as `SubagentEvent`: nine variants map 1:1 to the
//! frontend's `subagent_*` WS protocol; [`SubagentEvent::Usage`] is internal
//! accounting (session token counters) with no WS representation.

use std::future::Future;
use std::pin::Pin;

/// One task for a sub-agent to work on.
#[derive(Debug, Clone)]
pub struct SubagentSpawn {
    pub task: String,
}

/// Result of a completed sub-agent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub id: String,
    pub task: String,
    pub index: usize,
    /// Short headline (last assistant message), for event streams.
    pub summary: String,
    /// Full transcript of the sub-agent's assistant messages — the parent
    /// reasons over this, not over a tweet-length summary.
    pub output: String,
    pub tool_count: usize,
    pub error: Option<String>,
    /// Path of the sub-agent's own events.jsonl (persisted run), when the
    /// spawner logs sub-agents to disk.
    pub log_path: Option<String>,
}

/// Real-time events from sub-agent execution. The first nine variants map
/// 1:1 to the frontend's `subagent_*` WS messages (see
/// `web/src/types/index.ts`); [`Usage`](Self::Usage) is internal accounting
/// only (provider token counts folded into the session counters).
#[derive(Debug, Clone)]
pub enum SubagentEvent {
    AllStarted {
        count: usize,
    },
    Started {
        id: String,
        task: String,
        index: usize,
    },
    Thinking {
        id: String,
        content: String,
    },
    Content {
        id: String,
        content: String,
    },
    ToolStart {
        id: String,
        name: String,
        arguments: Option<String>,
    },
    ToolEnd {
        id: String,
        name: String,
        output: Option<String>,
    },
    Completed {
        id: String,
        index: usize,
        summary: String,
        tool_count: usize,
    },
    Error {
        id: String,
        index: usize,
        error: String,
    },
    Synthesizing,
    /// Provider-reported token usage from a sub-agent's LLM calls. Never
    /// forwarded to the frontend — the gateway/desktop accumulates it into
    /// the session's usage counters. Cache fields mirror the core `Usage`:
    /// 0 = the provider did not report a cache breakdown.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_write: u64,
    },
}

/// Trait injected into `ToolContext` by the host. The `spawn_subagents` tool
/// calls this to fan out parallel sub-agent loops. The host implementation
/// builds per-task contexts (same built-in tools/stream_fn/hooks, capped
/// max_turns) and runs `run_agent_loop` concurrently.
///
/// Event delivery is the spawner's own concern (configured at construction,
/// e.g. a WS forwarder): there is no per-call emit parameter — every caller
/// would only pass a no-op.
pub trait SubagentSpawner: Send + Sync {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>>;
}

/// A no-op spawner used when no host is wired (tests, bare agent_loop). Always
/// returns an error result explaining subagents are unavailable.
pub struct NoopSubagentSpawner;

impl SubagentSpawner for NoopSubagentSpawner {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
        Box::pin(async move {
            tasks
                .into_iter()
                .enumerate()
                .map(|(i, t)| SubagentResult {
                    id: format!("noop-{i}"),
                    task: t.task,
                    index: i + 1,
                    summary: String::new(),
                    output: String::new(),
                    tool_count: 0,
                    error: Some("subagents not available in this context".into()),
                    log_path: None,
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_spawner_returns_one_error_result_per_task() {
        let results = NoopSubagentSpawner
            .spawn(vec![
                SubagentSpawn {
                    task: "first".into(),
                },
                SubagentSpawn {
                    task: "second".into(),
                },
            ])
            .await;
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].id, "noop-0");
        assert_eq!(results[0].index, 1);
        assert_eq!(results[0].task, "first");
        assert!(
            results[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("subagents not available"),
            "got: {:?}",
            results[0].error
        );
        assert!(results[0].summary.is_empty());
        assert!(results[0].output.is_empty());
        assert_eq!(results[0].tool_count, 0);
        assert!(results[0].log_path.is_none());

        assert_eq!(results[1].id, "noop-1");
        assert_eq!(results[1].index, 2);
        assert_eq!(results[1].task, "second");
    }

    #[tokio::test]
    async fn noop_spawner_with_no_tasks_returns_nothing() {
        let results = NoopSubagentSpawner.spawn(Vec::new()).await;
        assert!(results.is_empty());
    }

    /// The trait is object-safe: a host wires the spawner as
    /// `Arc<dyn SubagentSpawner>` and calls `spawn` through it.
    #[tokio::test]
    async fn spawn_is_callable_through_the_trait_object() {
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(NoopSubagentSpawner);
        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "do a thing".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task, "do a thing");
        assert!(results[0].error.is_some());
    }
}
