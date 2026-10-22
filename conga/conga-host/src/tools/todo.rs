//! `todo` tool — the agent's multi-step working memory.
//!
//! Private state under `ToolContext.state_dir` (one JSON file per session);
//! moved into the built-in set because a harness agent needs task tracking
//! in every transport, not behind a CLI feature flag.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use conga::{ContentBlock, RiskLevel, ToolCallCtx, ToolDefinition, ToolError, ToolResult};

#[derive(Default, Serialize, Deserialize, Clone)]
struct Todo {
    id: u64,
    text: String,
    done: bool,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct State {
    todos: Vec<Todo>,
    next_id: u64,
}

fn load(ctx: &conga::ToolContext) -> State {
    let path = ctx.state_dir.join("todos.json");
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(ctx: &conga::ToolContext, state: &State) {
    let _ = std::fs::create_dir_all(&ctx.state_dir);
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = std::fs::write(ctx.state_dir.join("todos.json"), bytes);
    }
}

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "todo".into(),
        label: "Todo".into(),
        description: "Track multi-step work: add items before starting, toggle them done as you finish, list to review progress. State persists per session.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "list", "toggle", "clear"], "description": "add: create a todo; list: show all; toggle: flip done by id; clear: remove all" },
                "text": { "type": "string", "description": "todo text (action=add)" },
                "id": { "type": "integer", "description": "todo id (action=toggle)" }
            },
            "required": ["action"]
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, ToolError> {
    let action = ctx.args["action"].as_str().unwrap_or("list");
    let mut state = load(&ctx.ctx);

    let (text, is_error) = match action {
        "add" => match ctx.args["text"].as_str() {
            Some(t) if !t.trim().is_empty() => {
                state.next_id += 1;
                let id = state.next_id;
                state.todos.push(Todo {
                    id,
                    text: t.to_string(),
                    done: false,
                });
                save(&ctx.ctx, &state);
                (format!("added #{id}: {t}"), false)
            }
            _ => ("text is required for action=add".to_string(), true),
        },
        "list" => {
            if state.todos.is_empty() {
                ("(no todos)".to_string(), false)
            } else {
                let lines: Vec<String> = state
                    .todos
                    .iter()
                    .map(|t| {
                        format!(
                            "#{} [{}] {}",
                            t.id,
                            if t.done { "done" } else { "    " },
                            t.text
                        )
                    })
                    .collect();
                (lines.join("\n"), false)
            }
        }
        "toggle" => match ctx.args["id"].as_u64() {
            Some(id) => match state.todos.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.done = !t.done;
                    let desc = format!(
                        "#{} {} -> {}",
                        t.id,
                        t.text,
                        if t.done { "done" } else { "open" }
                    );
                    save(&ctx.ctx, &state);
                    (desc, false)
                }
                None => (format!("no todo #{id}"), true),
            },
            None => ("id is required for action=toggle".to_string(), true),
        },
        "clear" => {
            state.todos.clear();
            save(&ctx.ctx, &state);
            ("cleared".to_string(), false)
        }
        _ => (format!("unknown action: {action}"), true),
    };

    Ok(ToolResult {
        content: vec![ContentBlock::text(text)],
        details: serde_json::json!({"action": action}),
        is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    async fn run(args: serde_json::Value, state_dir: &std::path::Path) -> ToolResult {
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: state_dir.to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: state_dir.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    fn text_of(r: &ToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn add_list_toggle_clear_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"action": "add", "text": "first task"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert!(text_of(&r).contains("#1"));

        let r = run(
            serde_json::json!({"action": "add", "text": "second task"}),
            tmp.path(),
        )
        .await;
        assert!(text_of(&r).contains("#2"));

        let r = run(serde_json::json!({"action": "list"}), tmp.path()).await;
        let text = text_of(&r);
        assert!(text.contains("first task") && text.contains("second task"));

        let r = run(serde_json::json!({"action": "toggle", "id": 1}), tmp.path()).await;
        assert!(!r.is_error);
        assert!(text_of(&r).contains("done"));

        // State survives across calls (files under state_dir).
        let r = run(serde_json::json!({"action": "list"}), tmp.path()).await;
        assert!(text_of(&r).contains("[done]"));

        let r = run(serde_json::json!({"action": "clear"}), tmp.path()).await;
        assert!(!r.is_error);
        let r = run(serde_json::json!({"action": "list"}), tmp.path()).await;
        assert_eq!(text_of(&r), "(no todos)");
    }

    #[tokio::test]
    async fn add_requires_text() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"action": "add"}), tmp.path()).await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("text is required"));
    }

    #[tokio::test]
    async fn toggle_requires_known_id() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"action": "toggle", "id": 99}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    // ---- load / save ----

    fn ctx_for(state_dir: &std::path::Path) -> conga::ToolContext {
        ToolContext {
            cwd: state_dir.to_path_buf(),
            env: Default::default(),
            session_id: "s".into(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// Error path: no state file yet -> the default (empty) state, not a
    /// panic.
    #[test]
    fn load_returns_default_when_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = load(&ctx_for(tmp.path()));
        assert!(state.todos.is_empty());
        assert_eq!(state.next_id, 0);
    }

    /// Error path: an empty file is not valid JSON -> the default state.
    #[test]
    fn load_returns_default_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("todos.json"), "").unwrap();
        let state = load(&ctx_for(tmp.path()));
        assert!(state.todos.is_empty());
        assert_eq!(state.next_id, 0);
    }

    /// Error path: a truncated / corrupt file must not take the tool down —
    /// it falls back to the default state.
    #[test]
    fn load_returns_default_for_corrupt_json() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("todos.json"), "{not json").unwrap();
        let state = load(&ctx_for(tmp.path()));
        assert!(state.todos.is_empty());
        assert_eq!(state.next_id, 0);
    }

    /// Happy path: save -> load round-trips every field.
    #[test]
    fn save_then_load_round_trips_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_for(tmp.path());
        let state = State {
            todos: vec![
                Todo {
                    id: 1,
                    text: "first".into(),
                    done: false,
                },
                Todo {
                    id: 2,
                    text: "second".into(),
                    done: true,
                },
            ],
            next_id: 2,
        };
        save(&ctx, &state);
        let loaded = load(&ctx);
        assert_eq!(loaded.next_id, 2);
        assert_eq!(loaded.todos.len(), 2);
        assert_eq!(loaded.todos[0].text, "first");
        assert!(!loaded.todos[0].done);
        assert_eq!(loaded.todos[1].text, "second");
        assert!(loaded.todos[1].done);
    }

    /// Boundary: the state directory does not exist yet — save must create
    /// it (and the file) rather than silently dropping the state.
    #[test]
    fn save_creates_the_state_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("nested/state");
        let ctx = ctx_for(&state_dir);
        assert!(!state_dir.exists());
        save(
            &ctx,
            &State {
                todos: vec![Todo {
                    id: 1,
                    text: "x".into(),
                    done: false,
                }],
                next_id: 1,
            },
        );
        assert!(state_dir.join("todos.json").is_file());
        assert_eq!(load(&ctx).todos.len(), 1);
    }

    /// Error path: the state directory is a regular file, so the write
    /// cannot succeed. `save` must swallow the failure (a state write must
    /// never fail the tool) and leave the file untouched.
    #[test]
    fn save_failure_is_swallowed_and_leaves_no_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, "not a dir").unwrap();
        let ctx = ctx_for(&blocked);
        save(
            &ctx,
            &State {
                todos: vec![Todo {
                    id: 1,
                    text: "x".into(),
                    done: false,
                }],
                next_id: 1,
            },
        );
        assert_eq!(
            std::fs::read_to_string(&blocked).unwrap(),
            "not a dir",
            "the blocked path must be left alone"
        );
        assert!(load(&ctx).todos.is_empty());
    }

    /// The default action is `list`, so a call without an action must not
    /// error out.
    #[tokio::test]
    async fn missing_action_defaults_to_list() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({}), tmp.path()).await;
        assert!(!r.is_error);
        assert_eq!(text_of(&r), "(no todos)");
        assert_eq!(r.details["action"], "list");
    }

    /// Error path: an unknown action is reported, not ignored.
    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"action": "fly"}), tmp.path()).await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("unknown action: fly"));
    }

    /// Boundary: whitespace-only text is not a todo.
    #[tokio::test]
    async fn add_rejects_whitespace_only_text() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"action": "add", "text": "   \t "}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("text is required"));
    }

    /// Error path: toggle without a usable id.
    #[tokio::test]
    async fn toggle_requires_an_integer_id() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"action": "toggle"}), tmp.path()).await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("id is required"));

        let r = run(
            serde_json::json!({"action": "toggle", "id": "one"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("id is required"));
    }
}
