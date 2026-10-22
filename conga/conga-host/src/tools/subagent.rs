//! `spawn_subagents` tool — fan out parallel sub-agent loops.
//!
//! The spawner is captured in the execute closure at construction
//! ([`tool`]): each `ToolDefinition` owns its spawner, so hosts that build
//! separate tool lists (one per gateway connection) are isolated with zero
//! shared state. Built without a spawner (`built_in_tools()`), the tool
//! reports subagents as unavailable.

use std::sync::Arc;

use crate::subagent_types::{SubagentSpawn, SubagentSpawner};
use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

pub fn tool(spawner: Option<Arc<dyn SubagentSpawner>>) -> ToolDefinition {
    let spawner = spawner.unwrap_or_else(|| Arc::new(crate::subagent_types::NoopSubagentSpawner));
    ToolDefinition {
        name: "spawn_subagents".into(),
        label: "Spawn Subagents".into(),
        description: "Spawn parallel sub-agents to work on independent tasks concurrently. Each sub-agent runs its own agent loop with the standard built-in tools. Use for divide-and-conquer: searching multiple areas, writing + testing + reviewing in parallel. Max 5 tasks.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "task": { "type": "string" } }, "required": ["task"] },
                    "minItems": 1,
                    "maxItems": 5,
                    "description": "Each task gets its own sub-agent. Max 5."
                }
            },
            "required": ["tasks"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(move |ctx| Box::pin(execute(Arc::clone(&spawner), ctx))),
    }
}

async fn execute(
    spawner: Arc<dyn SubagentSpawner>,
    ctx: ToolCallCtx,
) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }

    let tasks = ctx.args["tasks"]
        .as_array()
        .ok_or_else(|| conga::error::ToolError::Message("tasks array is required".into()))?;

    let mut spawns: Vec<SubagentSpawn> = tasks
        .iter()
        .filter_map(|t| {
            t["task"].as_str().map(|s| SubagentSpawn {
                task: s.to_string(),
            })
        })
        .collect();
    // Malformed entries (no string `task`) are counted and reported below —
    // same no-silent-drops contract as the maxItems truncation.
    let invalid = tasks.len() - spawns.len();

    if spawns.is_empty() {
        return Ok(ToolResult::error(
            "no valid tasks provided (each needs a 'task' string)".to_string(),
        ));
    }

    // Enforce the schema's maxItems: 5, regardless of what the LLM sent.
    // Dropped tasks are reported so the model isn't silently truncated.
    let dropped = spawns.len().saturating_sub(5);
    spawns.truncate(5);

    let results = spawner.spawn(spawns).await;

    // Full sections: the parent reasons over the complete output, not a
    // 200-char summary. Log paths let it re-read a transcript on demand.
    let mut summary = results
        .iter()
        .map(|r| {
            let mut section = if let Some(err) = &r.error {
                format!("Subagent {} ({}): ERROR - {}", r.index, r.task, err)
            } else {
                format!("Subagent {} ({}):", r.index, r.task)
            };
            if !r.output.trim().is_empty() {
                section.push('\n');
                section.push_str(&r.output);
            } else if r.error.is_none() {
                section.push_str(&format!(" {}", r.summary));
            }
            if let Some(log) = &r.log_path {
                section.push_str(&format!("\n(full log: {log})"));
            }
            section
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    if dropped > 0 {
        summary.push_str(&format!(
            "\n\n(Note: {dropped} additional task(s) beyond the max of 5 were dropped.)"
        ));
    }
    if invalid > 0 {
        summary.push_str(&format!(
            "\n\n(Note: {invalid} task(s) without a valid 'task' string were skipped.)"
        ));
    }
    // The combined output can be huge; spill to disk past the cap.
    let summary = super::spill_or_truncate(&ctx, &summary);

    Ok(ToolResult {
        content: vec![ContentBlock::text(summary)],
        details: serde_json::json!({
            "subagent_count": results.len(),
            "completed": results.iter().filter(|r| r.error.is_none()).count(),
            "errors": results.iter().filter(|r| r.error.is_some()).count(),
            "log_paths": results.iter().filter_map(|r| r.log_path.clone()).collect::<Vec<_>>(),
            "dropped": dropped,
            "invalid": invalid,
        }),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::subagent_types::{SubagentResult, SubagentSpawner};
    use conga::types::tool::ToolContext;

    /// Records how many tasks it received; each result's summary carries
    /// the spawner's own marker (to tell spawners apart in tests).
    struct CountingSpawner {
        count: Arc<AtomicUsize>,
        marker: &'static str,
    }

    impl SubagentSpawner for CountingSpawner {
        fn spawn(
            &self,
            tasks: Vec<SubagentSpawn>,
        ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
            let count = Arc::clone(&self.count);
            let marker = self.marker;
            Box::pin(async move {
                count.fetch_add(tasks.len(), Ordering::SeqCst);
                tasks
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| SubagentResult {
                        id: format!("r-{i}"),
                        task: t.task,
                        index: i + 1,
                        summary: marker.into(),
                        output: String::new(),
                        tool_count: 0,
                        error: None,
                        log_path: None,
                    })
                    .collect()
            })
        }
    }

    fn ctx_with(tasks: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({ "tasks": tasks }),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: std::env::current_dir().unwrap(),
                env: HashMap::new(),
                session_id: "s1".into(),
                state_dir: std::env::temp_dir(),
            },
        }
    }
    fn seven_tasks() -> serde_json::Value {
        serde_json::json!([
            { "task": "a" },
            { "task": "b" },
            { "task": "c" },
            { "task": "d" },
            { "task": "e" },
            { "task": "f" },
            { "task": "g" },
        ])
    }
    fn result_text(r: &ToolResult) -> &str {
        r.content
            .first()
            .and_then(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The schema's maxItems (5) is enforced regardless of what the LLM
    /// sent, and the truncation is reported instead of being silent.
    #[tokio::test]
    async fn truncates_over_limit_tasks_and_reports() {
        let count = Arc::new(AtomicUsize::new(0));
        let spawner = Arc::new(CountingSpawner {
            count: Arc::clone(&count),
            marker: "ok",
        });
        let r = execute(spawner, ctx_with(seven_tasks())).await.unwrap();

        assert_eq!(
            count.load(Ordering::SeqCst),
            5,
            "spawner must receive max 5"
        );
        assert_eq!(r.details["subagent_count"], 5);
        assert_eq!(r.details["dropped"], 2);
        assert!(
            result_text(&r).contains("dropped"),
            "dropped count must be visible to the model"
        );
    }

    /// No spawner wired (bare agent_loop / CLI without subagents): every
    /// task comes back as an explicit error, never a silent no-op. Goes
    /// through `tool(None)`'s closure to lock the public Noop wiring.
    #[tokio::test]
    async fn no_spawner_reports_unavailable() {
        let t = tool(None);
        let r = (t.execute)(ctx_with(seven_tasks())).await.unwrap();

        assert_eq!(r.details["subagent_count"], 5);
        assert_eq!(r.details["errors"], 5);
        assert!(
            result_text(&r).contains("not available"),
            "unavailable spawner must be surfaced as errors"
        );
    }

    /// Malformed entries (no string `task`) are counted and reported, not
    /// silently dropped — same contract as the maxItems truncation.
    #[tokio::test]
    async fn malformed_tasks_are_reported() {
        let spawner = Arc::new(CountingSpawner {
            count: Arc::new(AtomicUsize::new(0)),
            marker: "ok",
        });
        let r = execute(
            spawner,
            ctx_with(serde_json::json!([{ "task": "a" }, { "nope": 1 }, { "task": "b" }])),
        )
        .await
        .unwrap();

        assert_eq!(r.details["subagent_count"], 2);
        assert_eq!(r.details["invalid"], 1);
        assert!(
            result_text(&r).contains("skipped"),
            "invalid task count must be visible to the model"
        );
    }

    /// Each tool owns the spawner captured at construction: two tools in
    /// the same process never see each other's spawner. (Regression for
    /// the former process-wide spawner slot, where the last install won.)
    #[tokio::test]
    async fn spawners_are_captured_per_tool() {
        let (c1, c2) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let t1 = tool(Some(Arc::new(CountingSpawner {
            count: Arc::clone(&c1),
            marker: "from-one",
        })));
        let t2 = tool(Some(Arc::new(CountingSpawner {
            count: Arc::clone(&c2),
            marker: "from-two",
        })));

        let tasks = serde_json::json!([{ "task": "x" }]);
        let r1 = (t1.execute)(ctx_with(tasks.clone())).await.unwrap();
        let r2 = (t2.execute)(ctx_with(tasks)).await.unwrap();

        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
        assert!(result_text(&r1).contains("from-one"));
        assert!(result_text(&r2).contains("from-two"));
    }

    /// Returns fixed results so the summary rendering can be asserted.
    struct ScriptedSpawner {
        results: Vec<SubagentResult>,
    }

    impl SubagentSpawner for ScriptedSpawner {
        fn spawn(
            &self,
            _tasks: Vec<SubagentSpawn>,
        ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
            let results = self.results.clone();
            Box::pin(async move { results })
        }
    }

    fn scripted_result(index: usize, task: &str) -> SubagentResult {
        SubagentResult {
            id: format!("r-{index}"),
            task: task.to_string(),
            index,
            summary: "headline".into(),
            output: String::new(),
            tool_count: 0,
            error: None,
            log_path: None,
        }
    }

    fn ctx_with_args(args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: std::env::current_dir().unwrap(),
                env: HashMap::new(),
                session_id: "s1".into(),
                state_dir: std::env::temp_dir(),
            },
        }
    }

    #[test]
    fn tool_definition_exposes_a_bounded_task_array() {
        let t = tool(None);
        assert_eq!(t.name, "spawn_subagents");
        assert_eq!(t.label, "Spawn Subagents");
        assert_eq!(t.risk, conga::types::tool::RiskLevel::Medium);
        let tasks = &t.parameters["properties"]["tasks"];
        assert_eq!(tasks["type"], "array");
        assert_eq!(tasks["minItems"], 1);
        assert_eq!(tasks["maxItems"], 5);
        assert_eq!(t.parameters["required"], serde_json::json!(["tasks"]));
    }

    /// Error path: the task array is mandatory.
    #[tokio::test]
    async fn requires_a_tasks_array() {
        let err = execute(Arc::new(crate::subagent_types::NoopSubagentSpawner), ctx_with_args(serde_json::json!({})))
            .await
            .expect_err("a missing tasks array must fail");
        assert!(
            err.to_string().contains("tasks array is required"),
            "{err}"
        );
    }

    /// Error path: an empty task list has nothing to spawn.
    #[tokio::test]
    async fn rejects_an_empty_task_list() {
        let r = execute(
            Arc::new(crate::subagent_types::NoopSubagentSpawner),
            ctx_with(serde_json::json!([])),
        )
        .await
        .unwrap();
        assert!(r.is_error);
        assert!(result_text(&r).contains("no valid tasks"), "{}", result_text(&r));
    }

    /// Error path: entries without a `task` string are all invalid.
    #[tokio::test]
    async fn rejects_tasks_without_a_task_string() {
        let r = execute(
            Arc::new(crate::subagent_types::NoopSubagentSpawner),
            ctx_with(serde_json::json!([{ "nope": 1 }, { "also": "nope" }])),
        )
        .await
        .unwrap();
        assert!(r.is_error);
        assert!(result_text(&r).contains("no valid tasks"), "{}", result_text(&r));
    }

    /// Error path: an aborted call must not spawn anything.
    #[tokio::test]
    async fn aborts_on_signal() {
        let mut ctx = ctx_with(serde_json::json!([{ "task": "a" }]));
        ctx.signal = Arc::new(AtomicBool::new(true));
        let r = execute(
            Arc::new(crate::subagent_types::NoopSubagentSpawner),
            ctx,
        )
        .await
        .unwrap();
        assert!(r.is_error);
        assert!(result_text(&r).contains("aborted"), "{}", result_text(&r));
    }

    /// The parent reasons over the FULL output, and the log path is named
    /// so the transcript can be re-read on demand.
    #[tokio::test]
    async fn includes_the_full_output_and_the_log_path() {
        let mut result = scripted_result(1, "do the thing");
        result.output = "the full transcript".into();
        result.log_path = Some("/tmp/subagent-1.jsonl".into());
        let spawner = Arc::new(ScriptedSpawner {
            results: vec![result],
        });
        let r = execute(spawner, ctx_with(serde_json::json!([{ "task": "do the thing" }])))
            .await
            .unwrap();
        assert!(!r.is_error);
        let text = result_text(&r);
        assert!(text.contains("the full transcript"), "{text}");
        assert!(text.contains("(full log: /tmp/subagent-1.jsonl)"), "{text}");
        assert_eq!(r.details["subagent_count"], 1);
        assert_eq!(r.details["completed"], 1);
        assert_eq!(r.details["errors"], 0);
        assert_eq!(r.details["log_paths"][0], "/tmp/subagent-1.jsonl");
    }

    /// A sub-agent that failed is reported as an error, never as a silent
    /// empty result.
    #[tokio::test]
    async fn reports_subagent_errors() {
        let mut result = scripted_result(1, "do the thing");
        result.error = Some("boom".into());
        let spawner = Arc::new(ScriptedSpawner {
            results: vec![result],
        });
        let r = execute(spawner, ctx_with(serde_json::json!([{ "task": "do the thing" }])))
            .await
            .unwrap();
        let text = result_text(&r);
        assert!(text.contains("ERROR - boom"), "{text}");
        assert_eq!(r.details["errors"], 1);
        assert_eq!(r.details["completed"], 0);
    }

    /// With no output the short summary stands in for it.
    #[tokio::test]
    async fn falls_back_to_the_summary_when_the_output_is_empty() {
        let spawner = Arc::new(ScriptedSpawner {
            results: vec![scripted_result(1, "do the thing")],
        });
        let r = execute(spawner, ctx_with(serde_json::json!([{ "task": "do the thing" }])))
            .await
            .unwrap();
        let text = result_text(&r);
        assert!(text.contains("Subagent 1 (do the thing)"), "{text}");
        assert!(text.contains("headline"), "{text}");
    }
}
