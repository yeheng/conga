//! `ToolDefinition` / `ToolFn` / `ToolContext` / `ToolResult` + hook verdicts.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::error::ToolError;
use crate::types::message::{ContentBlock, ToolResultMessage};

/// Risk level of a tool call - determines whether the host auto-approves,
/// prompts, or blocks based on the active permission mode. Lives on
/// [`ToolDefinition`] so the agent loop can forward it to hooks without a
/// hardcoded name table.
///
/// Grading rule: a tool that can execute arbitrary code or commands is
/// **always `High`** — no exceptions, no "but it's sandboxed". `bash` and
/// the PTY `terminal` tool must never diverge: a lower grade on either is
/// a privilege-escalation hole (the model will learn to prefer the tool
/// that skips the approver).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RiskLevel {
    Low,
    Medium,
    #[default]
    High,
}

/// A tool registered with the agent. `parameters` is a JSON Schema; the host
/// validates args before calling `execute`.
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub risk: RiskLevel,
    pub execute: ToolFn,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// The signature every tool's `execute` closure must match.
///
/// No `on_update` callback (V0.1 omits streaming tool progress — no consumer
/// exists among the 5 built-in tools).
pub type ToolFn = Arc<
    dyn Fn(ToolCallCtx) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>
        + Send
        + Sync,
>;

/// Arguments handed to a tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCallCtx {
    pub tool_call_id: String,
    pub args: serde_json::Value,
    pub signal: Arc<AtomicBool>,
    pub ctx: ToolContext,
}

impl ToolCallCtx {
    /// True if the caller has requested this invocation be cancelled. Tools
    /// should check this at entry and inside long loops, returning promptly
    /// (e.g. an "aborted" error) when set.
    pub fn aborted(&self) -> bool {
        self.signal.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Context passed into a tool. `state_dir` is this plugin's **private** state
/// directory (`~/.conga/sessions/{session_id}/tool_state/{tool_name}/`); the
/// tool reads/writes its own files there.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
    pub state_dir: PathBuf,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("session_id", &self.session_id)
            .field("state_dir", &self.state_dir)
            .finish()
    }
}

/// A tool's result. `details` is plugin-private (the agent never reads it).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(s)],
            details: serde_json::Value::Null,
            is_error: false,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(s)],
            details: serde_json::Value::Null,
            is_error: true,
        }
    }
}

/// Verdict returned by a `before_tool_call` hook — controls whether/how a tool
/// call proceeds.
#[derive(Debug, Clone)]
pub enum ToolCallVerdict {
    /// Let the call through unchanged.
    Allow,
    /// Refuse the call; `reason` becomes the ToolResult sent back to the LLM.
    Block(String),
    /// Replace the args, then execute.
    Modify(serde_json::Value),
}

/// Object-safe hook chain the agent loop consults around each tool call.
///
/// `before_tool_call` is async because hosts may need to ask a human for
/// approval (CLI: stdin; gateway: WebSocket round-trip). `after_tool_call`
/// stays sync — it is a pure transformation (redact etc.).
///
/// Cancellation contract: while the agent loop is suspended in
/// `before_tool_call().await`, an abort signal does NOT automatically cancel
/// the future. An implementor that may block on a human must check the abort
/// signal itself (or accept a cancel channel) and return promptly when set.
///
/// Defined in `types` (not `extension`) so `AgentLoopConfig` can hold an
/// `Option<Arc<dyn HookChain>>` without a circular dependency. The concrete
/// implementation is `ExtensionApiImpl`; `None` means "no hooks installed"
/// (the default — used by tests and the bare `agent_loop` helper).
pub trait HookChain: Send + Sync {
    /// Consult all `before_tool_call` handlers. First `Block` wins; otherwise
    /// the last `Modify` wins; default `Allow`.
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        risk: RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>>;

    /// Consult all `after_tool_call` handlers, each may replace the result.
    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::message::ContentBlock;

    fn tool_context() -> ToolContext {
        ToolContext {
            cwd: PathBuf::from("/tmp/work"),
            env: HashMap::from([("HOME".to_string(), "/tmp".to_string())]),
            session_id: "s1".to_string(),
            state_dir: PathBuf::from("/tmp/state"),
        }
    }

    fn tool_call_ctx(signal: Arc<AtomicBool>) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({"path": "a.txt"}),
            signal,
            ctx: tool_context(),
        }
    }

    // ── Debug formatting ──────────────────────────────────────────

    #[test]
    fn tool_definition_debug_shows_name_and_label_only() {
        let def = ToolDefinition {
            name: "read".to_string(),
            label: "Read".to_string(),
            description: "read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: Arc::new(|_c: ToolCallCtx| Box::pin(async { Ok(ToolResult::text("")) })),
        };
        let out = format!("{def:?}");
        assert!(out.contains("ToolDefinition"), "{out}");
        assert!(out.contains("read"), "{out}");
        assert!(out.contains("Read"), "{out}");
        // The description must not be dumped into debug logs.
        assert!(!out.contains("read a file"), "{out}");
    }

    #[test]
    fn tool_context_debug_shows_paths_and_env() {
        let out = format!("{:?}", tool_context());
        assert!(out.contains("ToolContext"), "{out}");
        assert!(out.contains("/tmp/work"), "{out}");
        assert!(out.contains("/tmp/state"), "{out}");
        assert!(out.contains("s1"), "{out}");
        assert!(out.contains("HOME"), "{out}");
    }

    // ── RiskLevel ─────────────────────────────────────────────────

    #[test]
    fn risk_level_default_is_high() {
        // `RiskLevel::default` is the fail-safe grade: an ungraded tool must
        // not be auto-approved.
        assert_eq!(RiskLevel::default(), RiskLevel::High);
        assert_ne!(RiskLevel::default(), RiskLevel::Low);
    }

    #[test]
    fn risk_level_debug_names_the_variant() {
        assert!(format!("{:?}", RiskLevel::Low).contains("Low"));
        assert!(format!("{:?}", RiskLevel::Medium).contains("Medium"));
        assert!(format!("{:?}", RiskLevel::High).contains("High"));
    }

    // ── ToolCallCtx::aborted ──────────────────────────────────────

    #[test]
    fn tool_call_ctx_not_aborted_when_signal_is_clear() {
        let ctx = tool_call_ctx(Arc::new(AtomicBool::new(false)));
        assert!(!ctx.aborted());
    }

    #[test]
    fn tool_call_ctx_aborted_when_signal_is_set() {
        let signal = Arc::new(AtomicBool::new(false));
        let ctx = tool_call_ctx(Arc::clone(&signal));
        assert!(!ctx.aborted());
        signal.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(ctx.aborted());
    }

    // ── ToolResult constructors ───────────────────────────────────

    #[test]
    fn tool_result_text_is_a_single_text_block_and_not_an_error() {
        let r = ToolResult::text("hello");
        assert_eq!(r.content.len(), 1);
        assert_eq!(r.content[0], ContentBlock::text("hello"));
        assert!(!r.is_error);
        assert!(r.details.is_null());
    }

    #[test]
    fn tool_result_text_accepts_empty_string() {
        let r = ToolResult::text("");
        assert_eq!(r.content, vec![ContentBlock::text("")]);
        assert!(!r.is_error);
    }

    #[test]
    fn tool_result_text_accepts_owned_string() {
        let owned = String::from("owned");
        let r = ToolResult::text(owned);
        assert_eq!(r.content[0], ContentBlock::text("owned"));
    }

    #[test]
    fn tool_result_error_sets_the_error_flag() {
        let r = ToolResult::error("boom");
        assert_eq!(r.content, vec![ContentBlock::text("boom")]);
        assert!(r.is_error);
        assert!(r.details.is_null());
    }

    #[test]
    fn tool_result_error_accepts_empty_string() {
        let r = ToolResult::error("");
        assert!(r.is_error);
        assert_eq!(r.content, vec![ContentBlock::text("")]);
    }

    // ── HookChain ─────────────────────────────────────────────────

    struct RecordingHookChain {
        before_calls: std::sync::Mutex<Vec<(String, String, serde_json::Value, RiskLevel)>>,
        verdict: ToolCallVerdict,
        redact: bool,
    }

    impl HookChain for RecordingHookChain {
        fn before_tool_call<'a>(
            &'a self,
            tool_call_id: &'a str,
            tool_name: &'a str,
            args: &'a serde_json::Value,
            risk: RiskLevel,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            self.before_calls.lock().unwrap().push((
                tool_call_id.to_string(),
                tool_name.to_string(),
                args.clone(),
                risk,
            ));
            let verdict = self.verdict.clone();
            Box::pin(async move { verdict })
        }

        fn after_tool_call(
            &self,
            tool_call_id: &str,
            result: &ToolResultMessage,
        ) -> ToolResultMessage {
            if !self.redact {
                return result.clone();
            }
            let mut out = result.clone();
            out.content = vec![ContentBlock::text(format!(
                "redacted:{tool_call_id}:{}",
                out.content.len()
            ))];
            out
        }
    }

    #[tokio::test]
    async fn hook_chain_before_tool_call_receives_call_metadata() {
        let chain = RecordingHookChain {
            before_calls: std::sync::Mutex::new(Vec::new()),
            verdict: ToolCallVerdict::Allow,
            redact: false,
        };
        let args = serde_json::json!({"path": "a.txt"});
        let verdict = chain
            .before_tool_call("call-1", "read", &args, RiskLevel::High)
            .await;
        assert!(matches!(verdict, ToolCallVerdict::Allow));
        let calls = chain.before_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call-1");
        assert_eq!(calls[0].1, "read");
        assert_eq!(calls[0].2, args);
        assert_eq!(calls[0].3, RiskLevel::High);
    }

    #[tokio::test]
    async fn hook_chain_before_tool_call_can_block_with_a_reason() {
        let chain = RecordingHookChain {
            before_calls: std::sync::Mutex::new(Vec::new()),
            verdict: ToolCallVerdict::Block("denied by policy".to_string()),
            redact: false,
        };
        let verdict = chain
            .before_tool_call("call-1", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        match verdict {
            ToolCallVerdict::Block(reason) => assert_eq!(reason, "denied by policy"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hook_chain_before_tool_call_can_modify_args() {
        let chain = RecordingHookChain {
            before_calls: std::sync::Mutex::new(Vec::new()),
            verdict: ToolCallVerdict::Modify(serde_json::json!({"path": "safe.txt"})),
            redact: false,
        };
        let verdict = chain
            .before_tool_call("call-1", "read", &serde_json::json!({}), RiskLevel::Low)
            .await;
        match verdict {
            ToolCallVerdict::Modify(args) => {
                assert_eq!(args, serde_json::json!({"path": "safe.txt"}))
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn hook_chain_after_tool_call_can_replace_the_result() {
        let chain = RecordingHookChain {
            before_calls: std::sync::Mutex::new(Vec::new()),
            verdict: ToolCallVerdict::Allow,
            redact: true,
        };
        let result = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::text("secret")],
            is_error: false,
            timestamp: 0,
        };
        let out = chain.after_tool_call("call-1", &result);
        assert_eq!(out.content, vec![ContentBlock::text("redacted:call-1:1")]);
        // Identity fields survive the rewrite.
        assert_eq!(out.tool_call_id, "call-1");
        assert_eq!(out.tool_name, "read");
    }

    #[test]
    fn hook_chain_after_tool_call_passes_through_when_not_redacting() {
        let chain = RecordingHookChain {
            before_calls: std::sync::Mutex::new(Vec::new()),
            verdict: ToolCallVerdict::Allow,
            redact: false,
        };
        let result = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
            timestamp: 7,
        };
        let out = chain.after_tool_call("call-1", &result);
        assert_eq!(out, result);
    }
}
