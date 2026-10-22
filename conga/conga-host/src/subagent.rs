//! Host-side `SubagentSpawner`: fans out parallel sub-agent loops.
//!
//! Each sub-agent runs its own `run_agent_loop` with the tool set,
//! stream_fn, and policy hooks passed at construction. Hosts pass the
//! built-ins minus `spawn_subagents` (nesting is disabled: the filtered
//! set simply has no spawning tool).
//! Events are mapped from `AgentEvent` to `SubagentEvent` and emitted through
//! the constructor-injected forwarder (`with_ws_emit`). Results are collected
//! after all sub-agents finish.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::subagent_types::{SubagentEvent, SubagentResult, SubagentSpawn, SubagentSpawner};
use conga::{
    run_agent_loop, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, CancelSignal,
    ContentBlock, ContentDelta, SessionEvent, ToolDefinition, UserMessage,
};

/// Max turns for a sub-agent (lower than the parent's default 50).
const SUBAGENT_MAX_TURNS: usize = 10;

/// A spawner built from the host's config. Each `spawn` call creates fresh
/// per-task contexts and runs them concurrently.
pub struct HostSubagentSpawner {
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    hooks: Arc<dyn conga::HookChain>,
    signal: CancelSignal,
    cwd: std::path::PathBuf,
    /// Process environment captured once at construction; cloned (cheap Arc)
    /// into each sub-agent context instead of re-querying the OS per task.
    env: Arc<HashMap<String, String>>,
    /// Loop-config template derived from the parent's provider/tunables
    /// (`build_loop_config`). Each sub-agent clones it and overrides
    /// `max_turns` (capped) plus signal/hooks. Sub-agents therefore inherit
    /// the parent's configured thinking level, per-turn tool-call ceiling,
    /// and retry policy — no hardcoded drift.
    loop_config: AgentLoopConfig,
    /// Optional event-forwarder set by the gateway. All subagent events are
    /// delivered through this callback (the trait has no per-call emit).
    ws_emit: Option<Arc<dyn Fn(SubagentEvent) + Send + Sync>>,
    /// Where sub-agent event logs live: `<root>/<sub-agent-id>/events.jsonl`
    /// (the parent session's `sub/` directory). `None` = in-memory only
    /// (tests, bare spawners).
    sub_log_root: Option<std::path::PathBuf>,
}

impl HostSubagentSpawner {
    pub fn new(
        system_prompt: String,
        tools: Vec<ToolDefinition>,
        hooks: Arc<dyn conga::HookChain>,
        signal: CancelSignal,
        cwd: std::path::PathBuf,
        loop_config: AgentLoopConfig,
    ) -> Self {
        Self {
            system_prompt,
            tools,
            hooks,
            signal,
            cwd,
            env: Arc::new(std::env::vars().collect()),
            loop_config,
            ws_emit: None,
            sub_log_root: None,
        }
    }

    /// Set an event forwarder (gateway). All subagent events are delivered to
    /// this callback.
    pub fn with_ws_emit(mut self, ws_emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>) -> Self {
        self.ws_emit = Some(ws_emit);
        self
    }

    /// Persist every sub-agent run under `<root>/<id>/events.jsonl`. Crash
    /// recovery and post-hoc inspection (the parent can `read` the log).
    pub fn with_sub_log_root(mut self, root: std::path::PathBuf) -> Self {
        self.sub_log_root = Some(root);
        self
    }
}

impl SubagentSpawner for HostSubagentSpawner {
    fn spawn(
        &self,
        tasks: Vec<SubagentSpawn>,
    ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
        let count = tasks.len();
        let emit: Arc<dyn Fn(SubagentEvent) + Send + Sync> = match &self.ws_emit {
            Some(ws) => Arc::clone(ws),
            None => Arc::new(|_| {}),
        };
        let spawner = Arc::new(HostSubagentSpawner {
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            hooks: Arc::clone(&self.hooks),
            signal: self.signal.clone(),
            cwd: self.cwd.clone(),
            env: Arc::clone(&self.env),
            loop_config: self.loop_config.clone(),
            ws_emit: self.ws_emit.clone(),
            sub_log_root: self.sub_log_root.clone(),
        });

        Box::pin(async move {
            emit(SubagentEvent::AllStarted { count });

            // Sub-agent tasks live in a JoinSet: dropping this future
            // mid-flight (turn cancelled, connection closed) drops the set
            // with it, aborting every still-running task at whatever await
            // point it sits — including the one being collected below. No
            // hand-rolled AbortHandle tracking to get right.
            let mut set: tokio::task::JoinSet<SubagentResult> = tokio::task::JoinSet::new();

            for (i, task) in tasks.into_iter().enumerate() {
                let id = uuid::Uuid::new_v4().to_string();
                let index = i + 1;
                let task_clone = task.task.clone();

                let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

                // Forwarder: AgentEvent → SubagentEvent → emit.
                let emit_fwd = Arc::clone(&emit);
                let fwd_id = id.clone();
                let fwd_handle = tokio::spawn(async move {
                    while let Some(ev) = event_rx.recv().await {
                        if let Some(sub_ev) = map_agent_event(&fwd_id, &ev) {
                            emit_fwd(sub_ev);
                        }
                    }
                });

                let sub_context = AgentContext {
                    system_prompt: spawner.system_prompt.clone(),
                    messages: vec![],
                    tools: spawner.tools.clone(),
                    cwd: spawner.cwd.clone(),
                    env: (*spawner.env).clone(),
                    session_id: format!("subagent-{id}"),
                };

                // Clone the parent-derived template, then pin this spawner's
                // signal/hooks and cap max_turns below the parent's.
                let mut sub_config = spawner.loop_config.clone();
                sub_config.max_turns = SUBAGENT_MAX_TURNS.min(spawner.loop_config.max_turns);
                sub_config.signal = Some(spawner.signal.clone());
                sub_config.hooks = Some(Arc::clone(&spawner.hooks));

                // Optional per-run event log: `<root>/<id>/events.jsonl`.
                // The persist callback is sync (std fs, same as the main
                // session's), so no runtime bridging.
                let log_path = spawner.sub_log_root.as_ref().map(|root| {
                    let storage = conga::EventStorage::new(root);
                    let events_path = root.join(&id).join("events.jsonl");
                    let log_id = id.clone();
                    sub_config.persist = Some(Arc::new(move |ev: &SessionEvent| {
                        storage.append_event_sync(&log_id, ev)
                    }));
                    events_path
                });
                // The loop persists Assistant/ToolResult only (initial
                // prompts are seeded pre-loop by contract); frame the run
                // so the log is self-contained.
                if let (Some(storage_root), false) =
                    (spawner.sub_log_root.as_ref(), task.task.is_empty())
                {
                    let storage = conga::EventStorage::new(storage_root);
                    let _ = storage.append_event_sync(
                        &id,
                        &SessionEvent::User(AgentMessage::User(UserMessage {
                            content: vec![ContentBlock::text(task.task.clone())],
                            timestamp: conga::now(),
                        })),
                    );
                }

                let user_msg = AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::text(task.task.clone())],
                    timestamp: conga::now(),
                });

                emit(SubagentEvent::Started {
                    id: id.clone(),
                    task: task_clone.clone(),
                    index,
                });

                let run_id = id.clone();
                let run_task = task_clone.clone();
                let run_index = index;
                let emit = Arc::clone(&emit);
                // Panic reporting happens inside the task (catch_unwind
                // below), so the metadata needed to report it is cloned
                // up front.
                let panic_emit = Arc::clone(&emit);
                let panic_id = run_id.clone();
                let panic_task = run_task.clone();
                let panic_log = log_path.clone().map(|p| p.display().to_string());
                set.spawn(async move {
                    let run = async move {
                        let result =
                            run_agent_loop(vec![user_msg], sub_context, sub_config, move |ev| {
                                let _ = event_tx.send(ev);
                            })
                            .await;

                        // Wait for forwarder to drain.
                        let _ = fwd_handle.await;

                        match result {
                            Ok(msgs) => {
                                // A run can end "successfully" with a failed
                                // stream: provider error mid-response, or abort via
                                // cancel. StopReason::Error/Aborted is surfaced as
                                // a sub-agent error, not a completion — otherwise
                                // the main agent and the frontend see a green
                                // checkmark on work that never finished.
                                let failed = msgs.iter().rev().find_map(|m| match m {
                                    AgentMessage::Assistant(a) => match &a.stop_reason {
                                        conga::StopReason::Error(e) => {
                                            Some(format!("sub-agent stream failed: {e}"))
                                        }
                                        conga::StopReason::Aborted => {
                                            Some("sub-agent cancelled".into())
                                        }
                                        _ => None,
                                    },
                                    _ => None,
                                });
                                if let Some(err_msg) = failed {
                                    emit(SubagentEvent::Error {
                                        id: run_id.clone(),
                                        index: run_index,
                                        error: err_msg.clone(),
                                    });
                                    SubagentResult {
                                        id: run_id,
                                        task: run_task,
                                        index: run_index,
                                        summary: String::new(),
                                        output: String::new(),
                                        tool_count: 0,
                                        error: Some(err_msg),
                                        log_path: log_path.clone().map(|p| p.display().to_string()),
                                    }
                                } else {
                                    let (summary, tool_count, output) =
                                        extract_summary_tools_output(&msgs);
                                    emit(SubagentEvent::Completed {
                                        id: run_id.clone(),
                                        index: run_index,
                                        summary: summary.clone(),
                                        tool_count,
                                    });
                                    SubagentResult {
                                        id: run_id,
                                        task: run_task,
                                        index: run_index,
                                        summary,
                                        output,
                                        tool_count,
                                        error: None,
                                        log_path: log_path.clone().map(|p| p.display().to_string()),
                                    }
                                }
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                emit(SubagentEvent::Error {
                                    id: run_id.clone(),
                                    index: run_index,
                                    error: err_msg.clone(),
                                });
                                SubagentResult {
                                    id: run_id,
                                    task: run_task,
                                    index: run_index,
                                    summary: String::new(),
                                    output: String::new(),
                                    tool_count: 0,
                                    error: Some(err_msg),
                                    log_path: log_path.clone().map(|p| p.display().to_string()),
                                }
                            }
                        }
                    };
                    // A panicking sub-agent still yields one result per spawn
                    // (frontend completion counts stay consistent); caught
                    // here so id/index/task are still in scope.
                    match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(run))
                        .await
                    {
                        Ok(result) => result,
                        Err(panic) => {
                            let err_msg = format!(
                                "subagent task panicked: {}",
                                panic_message(panic.as_ref())
                            );
                            panic_emit(SubagentEvent::Error {
                                id: panic_id.clone(),
                                index: run_index,
                                error: err_msg.clone(),
                            });
                            SubagentResult {
                                id: panic_id,
                                task: panic_task,
                                index: run_index,
                                summary: String::new(),
                                output: String::new(),
                                tool_count: 0,
                                error: Some(err_msg),
                                log_path: panic_log,
                            }
                        }
                    }
                });
            }

            // join_next() yields in completion order; restore declaration
            // order so the parent (and the frontend) read results as spawned.
            let mut results = Vec::with_capacity(set.len());
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(r) => results.push(r),
                    // Unreachable in practice: panics were converted to
                    // error results inside the task, and nothing aborts
                    // individual tasks. Kept as a safety net so a lost
                    // task still yields one result slot instead of a
                    // silent gap.
                    Err(e) => {
                        tracing::error!(error = %e, "subagent join failed");
                        results.push(SubagentResult {
                            id: String::new(),
                            task: String::new(),
                            index: usize::MAX,
                            summary: String::new(),
                            output: String::new(),
                            tool_count: 0,
                            error: Some(format!("subagent task lost: {e}")),
                            log_path: None,
                        });
                    }
                }
            }
            results.sort_by_key(|r| r.index);

            // All sub-agents finished: signal the main agent is synthesizing
            // their results. Must come AFTER all handles complete, not before.
            emit(SubagentEvent::Synthesizing);
            results
        })
    }
}

/// Best-effort panic payload text (`panic!("...")` payloads are `&str` or
/// `String`; anything else degrades to a placeholder).
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".into()
    }
}

/// Map a sub-agent's `AgentEvent` to a `SubagentEvent` tagged with `id`.
fn map_agent_event(id: &str, ev: &AgentEvent) -> Option<SubagentEvent> {
    match ev {
        AgentEvent::MessageUpdate { delta } => match delta {
            ContentDelta::TextDelta(t) => Some(SubagentEvent::Content {
                id: id.into(),
                content: t.clone(),
            }),
            ContentDelta::ThinkingDelta(t) => Some(SubagentEvent::Thinking {
                id: id.into(),
                content: t.clone(),
            }),
            ContentDelta::ToolCallDelta { .. } => None,
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => Some(SubagentEvent::ToolStart {
            id: id.into(),
            name: tool_name.clone(),
            arguments: Some(serde_json::to_string(args).unwrap_or_default()),
        }),
        AgentEvent::ToolExecutionEnd { result, .. } => {
            let output = result
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            // Note: the server's tool_call_id is deliberately NOT forwarded —
            // the frontend generates its own tool ids and matches by name
            // (sub-agents execute tools serially, so the name is unambiguous).
            Some(SubagentEvent::ToolEnd {
                id: id.into(),
                name: result.tool_name.clone(),
                output: Some(output),
            })
        }
        // Provider usage from a sub-agent's LLM calls: internal accounting
        // only — the gateway folds it into the session token counters.
        // Unreported cache (`None`) maps to 0 = not reported.
        AgentEvent::AfterProviderResponse { response, .. } => {
            response.usage.as_ref().map(|u| SubagentEvent::Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read: u.cache_read_tokens.unwrap_or(0),
                cache_write: u.cache_write_tokens.unwrap_or(0),
            })
        }
        _ => None,
    }
}

/// Extract the sub-agent's summary (≤200 chars), FULL assistant transcript,
/// and tool-result count. The parent reasons over `output`; `summary` is
/// for event streams and completion toasts.
fn extract_summary_tools_output(msgs: &[AgentMessage]) -> (String, usize, String) {
    let tool_count = msgs
        .iter()
        .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
        .count();

    // Full output: every assistant text block, in order, separated.
    let output: Vec<String> = msgs
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        })
        .collect();
    let output = output.join("\n\n");

    let summary = match msgs.iter().rev().find_map(|m| match m {
        AgentMessage::Assistant(a) => Some(a),
        _ => None,
    }) {
        Some(a) => {
            let text: String = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if matches!(a.stop_reason, conga::StopReason::ToolUse) {
                // The loop exhausted max_turns mid-tool-call.
                if text.is_empty() {
                    "(reached turn limit without a final answer)".into()
                } else {
                    let truncated: String = text.chars().take(200).collect();
                    format!("{truncated} (note: reached turn limit, result may be incomplete)")
                }
            } else {
                text.chars().take(200).collect()
            }
        }
        None => String::new(),
    };

    (summary, tool_count, output)
}
#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    use conga::{
        AgentLoopConfig, ModelSpec, ProviderApi, RetryPolicy, StreamChunk, StreamFn, ToolDefinition,
    };

    use crate::hooks::HookStack;
    use crate::permission::{Approver, Mode, PermissionPolicy};

    /// A fresh signal that is already cancelled.
    fn cancelled_signal() -> conga::CancelSignal {
        let sig = conga::CancelSignal::new();
        sig.cancel();
        sig
    }

    /// A mock StreamFn that replays a fixed chunk sequence on every call.
    struct MockStream(Vec<StreamChunk>);

    impl StreamFn for MockStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            Box::pin(futures_util::stream::iter(self.0.clone()))
        }
    }

    fn test_spawner(
        stream: Arc<dyn StreamFn>,
        hooks: Arc<dyn conga::HookChain>,
        ev_tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
        signal: conga::CancelSignal,
    ) -> HostSubagentSpawner {
        HostSubagentSpawner::new(
            "sys".into(),
            crate::built_in_tools(),
            hooks,
            signal,
            std::env::current_dir().unwrap(),
            AgentLoopConfig {
                model: ModelSpec {
                    id: "test".into(),
                    api: ProviderApi::OpenAiCompat,
                    max_tokens: 1024,
                },
                max_turns: 2,
                max_tool_calls_per_turn: 20,
                tool_timeout: None,
                signal: None,
                stream_fn: stream,
                hooks: None,
                retry: RetryPolicy::off(),
                persist: None,
                steer: None,
                transform_context: None,
            },
        )
        .with_ws_emit(Arc::new(move |ev| {
            let _ = ev_tx.send(ev);
        }))
    }

    /// Persistence: with a sub log root set, every sub-agent run lands in
    /// `<root>/<id>/events.jsonl` (User + Assistant rows), and the result
    /// carries the FULL assistant transcript, not a truncated summary.
    #[tokio::test]
    async fn subagent_run_persists_and_returns_full_output() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let tmp = tempfile::tempdir().unwrap();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("part one. ".into()),
                StreamChunk::TextDelta("part two.".into()),
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        )
        .with_sub_log_root(tmp.path().to_path_buf());

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "write a long report".into(),
            }])
            .await;

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.output, "part one. part two.");

        let log = r.log_path.as_ref().expect("log path set");
        let log = std::path::Path::new(log);
        assert!(log.is_file(), "sub log must exist: {}", log.display());
        let raw = std::fs::read_to_string(log).unwrap();
        assert!(
            raw.contains("\"user\""),
            "log must hold the task user message: {raw}"
        );
        assert!(
            raw.contains("\"assistant\""),
            "log must hold the assistant reply: {raw}"
        );
    }

    /// Security regression: a sub-agent calling `bash` must go through the
    #[tokio::test]
    async fn subagent_tool_calls_go_through_shared_policy() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_c = Arc::clone(&calls);
        let approver: Approver = Arc::new(move |name: &str, _args: &serde_json::Value| {
            let calls = Arc::clone(&calls_c);
            Box::pin(async move {
                calls.lock().push(name.to_string());
                false // deny: AutoEdit consults the approver for High-risk tools
            })
        });
        let policy = Arc::new(PermissionPolicy::new(Mode::AutoEdit, approver));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "t1".into(),
                    name: Some("bash".into()),
                    args_delta: "{\"command\":\"echo hi\"}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "run a command".into(),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            results[0].error.is_none(),
            "sub-agent should complete: {:?}",
            results[0].error
        );

        // The sub-agent's bash call hit the shared policy: the approver was
        // consulted with the tool name...
        let names = calls.lock().clone();
        assert!(
            names.iter().any(|n| n == "bash"),
            "approver must be consulted for sub-agent bash: {names:?}"
        );
        // ...and the block surfaced on the wire as a tool result.
        let mut tool_ends = Vec::new();
        while let Ok(ev) = ev_rx.try_recv() {
            if let SubagentEvent::ToolEnd { name, output, .. } = ev {
                tool_ends.push((name, output.unwrap_or_default()));
            }
        }
        assert!(
            tool_ends
                .iter()
                .any(|(n, o)| n == "bash" && o.contains("denied")),
            "blocked bash must be visible in events: {tool_ends:?}"
        );
    }

    /// A hook that blocks a tool by name must also gate SUB-AGENT calls:
    /// hosts pass a composed stack (extra hooks first, policy last) into
    /// the spawner, so an extra hook can never be bypassed by delegating
    /// the call to a sub-agent. (Regression: the spawner used to receive
    /// the policy alone.)
    #[tokio::test]
    async fn extra_hook_blocks_tool_inside_subagent_run() {
        struct BlockBashByHook;
        impl conga::HookChain for BlockBashByHook {
            fn before_tool_call<'a>(
                &'a self,
                _: &'a str,
                name: &'a str,
                _: &'a serde_json::Value,
                _: conga::RiskLevel,
            ) -> Pin<Box<dyn Future<Output = conga::ToolCallVerdict> + Send + 'a>> {
                Box::pin(async move {
                    if name == "bash" {
                        conga::ToolCallVerdict::Block("no bash by extra hook".into())
                    } else {
                        conga::ToolCallVerdict::Allow
                    }
                })
            }
            fn after_tool_call(
                &self,
                _: &str,
                r: &conga::ToolResultMessage,
            ) -> conga::ToolResultMessage {
                r.clone()
            }
        }

        // Same composition order as the main agent: extra hooks, then policy.
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto, // policy alone would ALLOW; only the hook blocks
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> =
            Arc::new(HookStack::new(vec![Arc::new(BlockBashByHook), policy]));

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "t1".into(),
                    name: Some("bash".into()),
                    args_delta: "{\"command\":\"echo hi\"}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "run a command".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_none());

        let mut tool_ends = Vec::new();
        while let Ok(ev) = ev_rx.try_recv() {
            if let SubagentEvent::ToolEnd { name, output, .. } = ev {
                tool_ends.push((name, output.unwrap_or_default()));
            }
        }
        assert!(
            tool_ends
                .iter()
                .any(|(n, o)| n == "bash" && o.contains("no bash by extra hook")),
            "the extra hook's block must surface inside the sub-agent: {tool_ends:?}"
        );
    }

    /// Protocol ordering: `Synthesizing` must arrive after every sub-agent
    /// terminal event; `AllStarted` leads; usage is forwarded internally.
    #[tokio::test]
    async fn synthesizing_arrives_last() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("done".into()),
                StreamChunk::Usage {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_write: 0,
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "say done".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].error.is_none());

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(matches!(
            events.first(),
            Some(SubagentEvent::AllStarted { .. })
        ));
        assert!(matches!(events.last(), Some(SubagentEvent::Synthesizing)));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SubagentEvent::Completed { .. }))
                .count(),
            1,
            "events: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SubagentEvent::Usage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..
                }
            )),
            "usage must be emitted: {events:?}"
        );
    }

    /// Summary honesty: when a sub-agent exhausts max_turns mid-tool-call,
    /// the summary must NOT report an earlier turn's text as if it were the
    /// final answer — it must indicate the task is incomplete.
    #[tokio::test]
    async fn max_turns_exhaustion_marks_summary_incomplete() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        // Mock replays the same chunks every turn: text + tool call → the
        // loop keeps going until max_turns (2) runs out.
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("Searching the codebase".into()),
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "tc1".into(),
                    name: Some("list".into()),
                    args_delta: "{}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn {
                task: "search".into(),
            }])
            .await;
        assert_eq!(results.len(), 1);
        // It's Completed (not Error) — the sub-agent did work, just didn't finish.
        assert!(results[0].error.is_none(), "max_turns is not an error");
        // But the summary must warn about incompleteness, not report the
        // stale "Searching the codebase" text as the final answer.
        let summary = &results[0].summary;
        assert!(
            summary.contains("turn limit"),
            "summary must indicate incomplete: {summary}"
        );
    }

    /// Error reporting: a sub-agent whose provider stream dies mid-response
    /// must surface as an Error, not a hollow Completed.
    #[tokio::test]
    async fn mid_stream_failure_reports_error_not_completed() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("partial".into()),
                StreamChunk::Error("provider boom".into()),
            ])),
            hooks,
            ev_tx,
            conga::CancelSignal::new(),
        );

        let results = spawner
            .spawn(vec![SubagentSpawn { task: "t".into() }])
            .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0].error.is_some(),
            "mid-stream failure must be an error result: {:?}",
            results[0].error
        );

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Error { .. })),
            "a failed stream must emit Error: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Completed { .. })),
            "a failed stream must NOT emit Completed: {events:?}"
        );
    }

    /// Cancellation: a pre-set abort signal must surface as a sub-agent
    /// error ("cancelled"), not a hollow Completed.
    #[tokio::test]
    async fn aborted_subagent_reports_cancelled() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawner = test_spawner(
            Arc::new(MockStream(vec![
                StreamChunk::TextDelta("hi".into()),
                StreamChunk::Done,
            ])),
            hooks,
            ev_tx,
            cancelled_signal(), // pre-set: loop aborts before any provider call
        );

        let results = spawner
            .spawn(vec![SubagentSpawn { task: "t".into() }])
            .await;
        let err = results[0].error.clone();
        assert!(
            err.is_some(),
            "pre-set signal must produce an error result: {:?}",
            err
        );
        assert!(
            err.as_deref().unwrap_or_default().contains("cancelled"),
            "abort must read as cancelled: {err:?}"
        );

        let events: Vec<SubagentEvent> = std::iter::from_fn(|| ev_rx.try_recv().ok()).collect();
        assert!(events
            .iter()
            .any(|e| matches!(e, SubagentEvent::Error { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })));
    }

    /// Lifecycle: dropping the spawn future mid-flight must abort every
    /// sub-agent task (no detached tasks executing after the turn is gone).
    #[tokio::test]
    async fn dropping_spawn_future_aborts_subagents() {
        let policy = Arc::new(PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_, _| Box::pin(async { true })),
        ));
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy]));
        // The stream parks on a oneshot; if the sub-agent task survives the
        // spawn-future drop, the sender below stays alive and this test hangs
        // (it fails via the assertion instead).
        let (block_tx, block_rx) = tokio::sync::oneshot::channel::<()>();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let stream: Arc<dyn StreamFn> =
            Arc::new(BlockingStream(parking_lot::Mutex::new(Some(block_rx))));
        let spawner = test_spawner(stream, hooks, ev_tx, conga::CancelSignal::new());

        // `spawn()` already returns `Pin<Box<dyn Future>>` — no tokio::pin!
        // (it would shadow the variable, making `drop(fut)` drop only the
        // pin wrapper and leaving the future — and the abort guard — alive).
        let mut fut = spawner.spawn(vec![SubagentSpawn {
            task: "hang".into(),
        }]);
        // One poll: the async block spawns the sub-agent task synchronously,
        // then parks on the first handle.await.
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Dropping the spawn future must abort the sub-agent task, which
        // drops its pending stream future — and with it the oneshot receiver.
        // (The local `spawner` also holds the stream Arc; drop it so the
        // receiver's fate depends only on the aborted task.)
        drop(fut);
        drop(spawner);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            block_tx.send(()).is_err(),
            "sub-agent task must be aborted when the spawn future is dropped"
        );
    }

    /// A StreamFn that parks on a oneshot receiver (used by the abort test).
    struct BlockingStream(parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);

    impl StreamFn for BlockingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let rx = self.0.lock().take();
            Box::pin(futures_util::stream::once(async move {
                if let Some(rx) = rx {
                    let _ = rx.await; // parks until the task is aborted
                }
                StreamChunk::Done
            }))
        }
    }

    // ── panic payload rendering ──

    #[test]
    fn panic_message_reads_both_string_payloads() {
        let borrowed: &str = "borrowed boom";
        let p: &(dyn std::any::Any + Send) = &borrowed;
        assert_eq!(panic_message(p), "borrowed boom");

        let owned = String::from("owned boom");
        let p: &(dyn std::any::Any + Send) = &owned;
        assert_eq!(panic_message(p), "owned boom");
    }

    #[test]
    fn panic_message_degrades_for_a_non_string_payload() {
        let number = 42u32;
        let p: &(dyn std::any::Any + Send) = &number;
        assert_eq!(panic_message(p), "unknown panic payload");
    }

    // ── AgentEvent → SubagentEvent mapping ──

    fn assistant_msg(text: &str, stop_reason: conga::StopReason) -> conga::AssistantMessage {
        conga::AssistantMessage {
            content: vec![ContentBlock::text(text)],
            model: "m".into(),
            stop_reason,
            usage: None,
            timestamp: 1,
            stream_indices: Vec::new(),
        }
    }

    fn tool_result_msg(tool_name: &str, text: &str) -> conga::ToolResultMessage {
        conga::ToolResultMessage {
            tool_call_id: "tc1".into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text(text)],
            is_error: false,
            timestamp: 1,
        }
    }

    #[test]
    fn map_agent_event_maps_text_and_thinking_deltas() {
        let ev = AgentEvent::MessageUpdate {
            delta: ContentDelta::TextDelta("hi".into()),
        };
        match map_agent_event("s1", &ev) {
            Some(SubagentEvent::Content { id, content }) => {
                assert_eq!(id, "s1");
                assert_eq!(content, "hi");
            }
            other => panic!("expected Content, got {other:?}"),
        }

        let ev = AgentEvent::MessageUpdate {
            delta: ContentDelta::ThinkingDelta("hmm".into()),
        };
        match map_agent_event("s1", &ev) {
            Some(SubagentEvent::Thinking { id, content }) => {
                assert_eq!(id, "s1");
                assert_eq!(content, "hmm");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn map_agent_event_ignores_a_tool_call_delta() {
        let ev = AgentEvent::MessageUpdate {
            delta: ContentDelta::ToolCallDelta {
                id: "t1".into(),
                name: Some("bash".into()),
                args_delta: "{}".into(),
            },
        };
        assert!(
            map_agent_event("s1", &ev).is_none(),
            "a tool-call delta has no SubagentEvent counterpart"
        );
    }

    #[test]
    fn map_agent_event_maps_tool_start_and_end() {
        let ev = AgentEvent::ToolExecutionStart {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        };
        match map_agent_event("s1", &ev) {
            Some(SubagentEvent::ToolStart {
                id,
                name,
                arguments,
            }) => {
                assert_eq!(id, "s1");
                assert_eq!(name, "bash");
                let args: serde_json::Value =
                    serde_json::from_str(&arguments.expect("arguments must be set")).unwrap();
                assert_eq!(args["command"], "ls");
            }
            other => panic!("expected ToolStart, got {other:?}"),
        }

        let ev = AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc1".into(),
            result: tool_result_msg("bash", "done"),
            is_error: false,
        };
        match map_agent_event("s1", &ev) {
            Some(SubagentEvent::ToolEnd { id, name, output }) => {
                assert_eq!(id, "s1");
                assert_eq!(name, "bash");
                assert_eq!(output.as_deref(), Some("done"));
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    #[test]
    fn map_agent_event_maps_provider_usage_and_drops_unreported_usage() {
        let mut response = assistant_msg("x", conga::StopReason::EndTurn);
        response.usage = Some(conga::Usage {
            input_tokens: 5,
            output_tokens: 2,
            cache_read_tokens: None,
            cache_write_tokens: Some(3),
        });
        let ev = AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response,
        };
        match map_agent_event("s1", &ev) {
            Some(SubagentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read,
                cache_write,
            }) => {
                assert_eq!(input_tokens, 5);
                assert_eq!(output_tokens, 2);
                assert_eq!(cache_read, 0, "unreported cache reads are 0");
                assert_eq!(cache_write, 3);
            }
            other => panic!("expected Usage, got {other:?}"),
        }

        // No usage reported → no event at all.
        let ev = AgentEvent::AfterProviderResponse {
            model: "m".into(),
            response: assistant_msg("x", conga::StopReason::EndTurn),
        };
        assert!(map_agent_event("s1", &ev).is_none());
    }

    #[test]
    fn map_agent_event_ignores_lifecycle_events() {
        assert!(map_agent_event("s1", &AgentEvent::AgentStart).is_none());
        assert!(map_agent_event("s1", &AgentEvent::TurnStart).is_none());
        assert!(map_agent_event("s1", &AgentEvent::AgentEnd).is_none());
        assert!(map_agent_event("s1", &AgentEvent::MessageStart).is_none());
    }

    // ── summary / output extraction ──

    #[test]
    fn extract_summary_tools_output_on_empty_input() {
        let (summary, tool_count, output) = extract_summary_tools_output(&[]);
        assert_eq!(summary, "");
        assert_eq!(tool_count, 0);
        assert!(output.is_empty());
    }

    #[test]
    fn extract_summary_tools_output_counts_tools_and_joins_the_transcript() {
        let msgs = vec![
            AgentMessage::user("u"),
            AgentMessage::Assistant(assistant_msg("hello", conga::StopReason::EndTurn)),
            AgentMessage::ToolResult(tool_result_msg("f", "ok")),
            AgentMessage::Assistant(assistant_msg("world", conga::StopReason::EndTurn)),
        ];
        let (summary, tool_count, output) = extract_summary_tools_output(&msgs);
        assert_eq!(tool_count, 1);
        assert_eq!(output, "hello\n\nworld");
        assert_eq!(summary, "world", "the summary is the last assistant text");
    }

    #[test]
    fn extract_summary_tools_output_ignores_non_text_blocks() {
        let mut a = assistant_msg("", conga::StopReason::EndTurn);
        a.content = vec![ContentBlock::Thinking {
            thinking: "secret reasoning".into(),
        }];
        let msgs = vec![AgentMessage::Assistant(a)];
        let (summary, tool_count, output) = extract_summary_tools_output(&msgs);
        assert_eq!(summary, "");
        assert_eq!(output, "", "thinking blocks are not part of the output");
        assert_eq!(tool_count, 0);
    }

    #[test]
    fn extract_summary_tools_output_marks_turn_limit_exhaustion() {
        // No text at all → the placeholder names the turn limit.
        let msgs = vec![AgentMessage::Assistant(assistant_msg(
            "",
            conga::StopReason::ToolUse,
        ))];
        let (summary, _, _) = extract_summary_tools_output(&msgs);
        assert_eq!(summary, "(reached turn limit without a final answer)");

        // With text → the text is kept but flagged as incomplete.
        let msgs = vec![AgentMessage::Assistant(assistant_msg(
            "partial work",
            conga::StopReason::ToolUse,
        ))];
        let (summary, _, _) = extract_summary_tools_output(&msgs);
        assert!(summary.starts_with("partial work"), "got: {summary}");
        assert!(
            summary.contains("reached turn limit, result may be incomplete"),
            "got: {summary}"
        );
    }

    #[test]
    fn extract_summary_tools_output_truncates_the_summary_but_not_the_output() {
        let long = "x".repeat(500);
        let msgs = vec![AgentMessage::Assistant(assistant_msg(
            &long,
            conga::StopReason::EndTurn,
        ))];
        let (summary, _, output) = extract_summary_tools_output(&msgs);
        assert_eq!(summary.chars().count(), 200, "the summary is capped");
        assert_eq!(
            output.chars().count(),
            500,
            "the parent reasons over the FULL transcript"
        );
    }
}
