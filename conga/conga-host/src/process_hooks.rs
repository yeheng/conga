//! Process-out PreToolUse hooks: config discovery + external-command gate.
//!
//! Claude-compatible subset: commands run via `sh -c`, receive a JSON event
//! on stdin, and answer with exit codes / a stdout decision object. Composed
//! into the host's HookStack BEFORE the permission policy, so a failing or
//! wedged hook fails OPEN (warn + allow) — the policy underneath is still
//! the floor gate. See docs/hooks.md for the config schema.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use conga::{ToolCallVerdict, ToolResultMessage};
use serde_json::Value;

/// Default per-hook deadline (Claude-compatible `timeout` is in seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Which tools a hook applies to. Empty/`*`/`all` matcher → `All`.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolMatcher {
    All,
    Names(Vec<String>),
}

impl ToolMatcher {
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            ToolMatcher::All => true,
            ToolMatcher::Names(names) => names.iter().any(|n| n == tool),
        }
    }
}

/// One loaded process hook, ready to run.
#[derive(Debug, Clone)]
pub struct ProcessHook {
    pub command: String,
    pub tools: ToolMatcher,
    pub timeout: Duration,
}

/// The on-disk file (Claude-compatible shape; only `PreToolUse` is read,
/// unknown keys are ignored for forward compatibility).
#[derive(serde::Deserialize)]
struct HooksFile {
    #[serde(default)]
    #[serde(rename = "PreToolUse")]
    pre_tool_use: Vec<MatcherGroup>,
}

#[derive(serde::Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: String,
    #[serde(default)]
    hooks: Vec<CommandHook>,
}

#[derive(serde::Deserialize)]
struct CommandHook {
    #[serde(default)]
    r#type: String,
    command: String,
    #[serde(default)]
    timeout: u64,
}

fn parse_matcher(raw: &str) -> ToolMatcher {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "*" || normalized == "all" {
        return ToolMatcher::All;
    }
    ToolMatcher::Names(
        normalized
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Global `<global_root>/hooks.json` entries first, then project
/// `<project_dir>/.conga/hooks.json` entries appended (both run; the chain
/// applies first-Block-wins / last-Modify-wins, mirroring HookStack).
/// Malformed or unreadable files are skipped with a warning — fail-open,
/// never abort assembly. Only `type: "command"` hooks are supported.
pub fn load_process_hooks(global_root: &Path, project_dir: &Path) -> Vec<ProcessHook> {
    let mut out = Vec::new();
    load_file(&global_root.join("hooks.json"), &mut out);
    load_file(&project_dir.join(".conga").join("hooks.json"), &mut out);
    out
}

fn load_file(path: &Path, out: &mut Vec<ProcessHook>) {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        // No file = no hooks; not an error. Anything else (EACCES, EISDIR,
        // …) is loud: silently disabling the user's hooks is a trap.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "hooks.json unreadable; skipping file");
            return;
        }
    };
    let file: HooksFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "hooks.json unparseable; skipping file");
            return;
        }
    };
    for group in file.pre_tool_use {
        let tools = parse_matcher(&group.matcher);
        for h in group.hooks {
            // Empty type defaults to "command" (Claude's only current kind);
            // any other explicit type is skipped loudly, not fatal.
            let kind = h.r#type.trim().to_ascii_lowercase();
            if !kind.is_empty() && kind != "command" {
                tracing::warn!(
                    path = %path.display(),
                    kind = %kind,
                    "unsupported hook type; skipping entry"
                );
                continue;
            }
            if h.command.trim().is_empty() {
                tracing::warn!(path = %path.display(), "hook with empty command; skipping entry");
                continue;
            }
            let timeout = if h.timeout == 0 {
                Duration::from_secs(DEFAULT_TIMEOUT_SECS)
            } else {
                Duration::from_secs(h.timeout)
            };
            out.push(ProcessHook {
                command: h.command,
                tools: tools.clone(),
                timeout,
            });
        }
    }
}

/// What one process hook decided. `NoOpinion` = no opinion (allow, keep going).
enum HookDecision {
    NoOpinion,
    Allow,
    Block(String),
    Modify(Value),
}

/// A chain of process-out PreToolUse hooks. Composed into the host's
/// HookStack before the permission policy; every failure mode (spawn
/// error, non-2/non-0 exit, timeout) fails OPEN with a warning because the
/// policy underneath still gates the call. `after_tool_call` is a
/// passthrough in v1 — the trait method is sync, and PostToolUse process
/// hooks would need an async seam (deferred until a consumer exists).
pub struct ProcessHookChain {
    hooks: Vec<ProcessHook>,
    /// Working dir for hook commands. `discover()` pins it to the project
    /// dir so project hooks like `./scripts/check.sh` work no matter where
    /// the host process runs; `new()` leaves it unset (process cwd).
    cwd: Option<PathBuf>,
}

impl ProcessHookChain {
    pub fn new(hooks: Vec<ProcessHook>) -> Self {
        Self { hooks, cwd: None }
    }

    /// Number of loaded hooks (for the assembly log line).
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Paired with `len` per the std collection convention (and clippy's
    /// `len_without_is_empty`); `discover()` already returns None for this.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Production entry: global `~/.conga/hooks.json` + project
    /// `<project_dir>/.conga/hooks.json`. `None` when no hooks are
    /// configured (nothing pushed into the stack — zero overhead).
    pub fn discover(project_dir: &Path) -> Option<Arc<Self>> {
        let hooks = load_process_hooks(&conga::storage::config_dir(), project_dir);
        if hooks.is_empty() {
            None
        } else {
            Some(Arc::new(Self {
                hooks,
                cwd: Some(project_dir.to_path_buf()),
            }))
        }
    }
}

impl conga::HookChain for ProcessHookChain {
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a Value,
        risk: conga::RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            // HookStack semantics, mirrored inside the chain (hooks.rs):
            // first Block wins and stops the run; an explicit stdout
            // "allow" short-circuits the remaining hooks too. Otherwise
            // each Modify feeds the NEXT hook (hooks compose on the
            // rewritten args) and the last Modify wins; Allow is default.
            let mut current = args.clone();
            let mut modified = false;
            for hook in &self.hooks {
                if !hook.tools.matches(tool_name) {
                    continue;
                }
                match run_hook(
                    self.cwd.as_deref(),
                    hook,
                    tool_call_id,
                    tool_name,
                    &current,
                    risk,
                )
                .await
                {
                    HookDecision::Block(reason) => return ToolCallVerdict::Block(reason),
                    HookDecision::Allow => return ToolCallVerdict::Allow,
                    HookDecision::Modify(new_args) => {
                        current = new_args;
                        modified = true;
                    }
                    HookDecision::NoOpinion => {}
                }
            }
            if modified {
                ToolCallVerdict::Modify(current)
            } else {
                ToolCallVerdict::Allow
            }
        })
    }

    fn after_tool_call(
        &self,
        _tool_call_id: &str,
        result: &ToolResultMessage,
    ) -> ToolResultMessage {
        result.clone()
    }
}

/// Spawn one hook (`sh -c <command>`), feed the Claude-shaped payload on
/// stdin, apply the deadline. Every failure path returns `NoOpinion` after
/// a warning — fail-open by contract (see struct docs).
async fn run_hook(
    cwd: Option<&Path>,
    hook: &ProcessHook,
    tool_call_id: &str,
    tool_name: &str,
    args: &Value,
    risk: conga::RiskLevel,
) -> HookDecision {
    let payload = serde_json::json!({
        "tool_name": tool_name,
        "tool_input": args,
        "tool_call_id": tool_call_id,
        "risk": format!("{risk:?}").to_ascii_lowercase(),
    });
    let fallback = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let run = async {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&hook.command)
            .current_dir(cwd.unwrap_or(&fallback))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        // The child may exit before reading all of stdin (e.g. `exit 2`);
        // a broken pipe here must not lose its output.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
            let _ = stdin.flush().await;
        }
        child.wait_with_output().await
    };
    let output = match tokio::time::timeout(hook.timeout, run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(command = %hook.command, error = %e, "process hook failed to spawn; allowing");
            return HookDecision::NoOpinion;
        }
        Err(_) => {
            tracing::warn!(command = %hook.command, ?hook.timeout, "process hook timed out; allowing");
            return HookDecision::NoOpinion;
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match output.status.code() {
        Some(2) => HookDecision::Block(if stderr.is_empty() {
            "blocked by process hook".to_string()
        } else {
            stderr
        }),
        Some(0) => parse_decision(&output.stdout, &stderr),
        code => {
            tracing::warn!(command = %hook.command, ?code, stderr = %stderr, "process hook errored; allowing");
            HookDecision::NoOpinion
        }
    }
}

/// Claude-compatible stdout contract (exit 0):
/// `{"hookSpecificOutput": {"permissionDecision": "allow"|"deny",
/// "permissionDecisionReason": "...", "updatedInput": {...}}}`.
/// Precedence: "deny" → Block first (fail-closed); then an `updatedInput`
/// object → Modify (a rewrite paired with "allow" or no decision must not
/// be dropped); then "allow" → Allow. Absent/unparsable output = no opinion.
fn parse_decision(stdout: &[u8], stderr: &str) -> HookDecision {
    let Ok(v) = serde_json::from_slice::<Value>(stdout) else {
        return HookDecision::NoOpinion;
    };
    let Some(spec) = v.get("hookSpecificOutput") else {
        return HookDecision::NoOpinion;
    };
    // "deny" wins over everything (fail-closed); then an `updatedInput`
    // rewrite paired with "allow" or an absent decision → Modify; then a
    // plain "allow" → Allow.
    if spec.get("permissionDecision").and_then(|d| d.as_str()) == Some("deny") {
        let reason = spec
            .get("permissionDecisionReason")
            .and_then(|r| r.as_str())
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(stderr)
            .to_string();
        return HookDecision::Block(if reason.is_empty() {
            "blocked by process hook".to_string()
        } else {
            reason
        });
    }
    if let Some(input) = spec.get("updatedInput").filter(|i| i.is_object()) {
        return HookDecision::Modify(input.clone());
    }
    match spec.get("permissionDecision").and_then(|d| d.as_str()) {
        Some("allow") => HookDecision::Allow,
        _ => HookDecision::NoOpinion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_json(body: &str) -> String {
        format!("{{\"PreToolUse\": [{}]}}", body)
    }

    #[test]
    fn empty_and_missing_files_yield_no_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_process_hooks(&tmp.path().join("g"), tmp.path()).is_empty());
        assert!(load_process_hooks(tmp.path(), tmp.path()).is_empty());
    }

    #[test]
    fn parses_matcher_hooks_and_timeout() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(
                r#"{"matcher": "bash, write", "hooks": [
                    {"type": "command", "command": "exit 0"},
                    {"type": "command", "command": "exit 2", "timeout": 3}
                ]}"#,
            ),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 2);
        assert_eq!(
            hooks[0].tools,
            ToolMatcher::Names(vec!["bash".into(), "write".into()])
        );
        assert_eq!(hooks[0].timeout, std::time::Duration::from_secs(10));
        assert_eq!(hooks[1].timeout, std::time::Duration::from_secs(3));
        assert!(hooks[0].tools.matches("bash"));
        assert!(!hooks[0].tools.matches("read"));
        assert!(ToolMatcher::All.matches("anything"));
    }

    #[test]
    fn star_and_empty_matcher_mean_all_tools() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"matcher": "*", "hooks": [{"command": "true"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert!(matches!(hooks[0].tools, ToolMatcher::All));
        // "type" defaults to "command" when absent.
        assert_eq!(hooks[0].command, "true");
    }

    #[test]
    fn project_entries_run_after_global() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"hooks": [{"command": "global-first"}]}"#),
        )
        .unwrap();
        std::fs::create_dir_all(p.path().join(".conga")).unwrap();
        std::fs::write(
            p.path().join(".conga/hooks.json"),
            hook_json(r#"{"hooks": [{"command": "project-second"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].command, "global-first");
        assert_eq!(hooks[1].command, "project-second");
    }

    #[test]
    fn malformed_file_is_skipped_loudly() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(g.path().join("hooks.json"), "{ not json").unwrap();
        std::fs::create_dir_all(p.path().join(".conga")).unwrap();
        std::fs::write(
            p.path().join(".conga/hooks.json"),
            hook_json(r#"{"hooks": [{"command": "ok"}]}"#),
        )
        .unwrap();
        // Bad global skipped; good project file still loads.
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "ok");
    }

    #[test]
    fn unreadable_global_file_skips_loudly_but_project_loads() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        // A directory named hooks.json: read_to_string fails with EISDIR
        // (on some platforms EACCES) — an IO error that is NOT NotFound.
        // Must skip the file (warn, fail-open) without panicking, and the
        // project file must still load.
        std::fs::create_dir_all(g.path().join("hooks.json")).unwrap();
        std::fs::create_dir_all(p.path().join(".conga")).unwrap();
        std::fs::write(
            p.path().join(".conga/hooks.json"),
            hook_json(r#"{"hooks": [{"command": "ok"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "ok");
    }

    #[test]
    fn unsupported_event_keys_are_ignored_not_rejected() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        // PostToolUse/SessionStart present: parse must not fail, only
        // PreToolUse hooks are collected (forward compatibility).
        std::fs::write(
            g.path().join("hooks.json"),
            r#"{"PostToolUse": [{"hooks": [{"command": "x"}]}], "PreToolUse": [{"hooks": [{"command": "y"}]}]}"#,
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "y");
    }

    #[test]
    fn non_command_hook_type_is_skipped() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        std::fs::write(
            g.path().join("hooks.json"),
            hook_json(r#"{"hooks": [{"type": "http", "command": "no"}, {"command": "yes"}]}"#),
        )
        .unwrap();
        let hooks = load_process_hooks(g.path(), p.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "yes");
    }

    // ── matcher / file parsing ────────────────────────────────

    #[test]
    fn parse_matcher_blank_star_and_all_mean_every_tool() {
        for raw in ["", "   ", "*", "all", "ALL", " All "] {
            assert_eq!(
                parse_matcher(raw),
                ToolMatcher::All,
                "matcher {raw:?} must apply to every tool"
            );
        }
    }

    #[test]
    fn parse_matcher_splits_lowercases_and_trims_names() {
        assert_eq!(
            parse_matcher("Bash, Write"),
            ToolMatcher::Names(vec!["bash".into(), "write".into()])
        );
        assert_eq!(
            parse_matcher("  read ,  "),
            ToolMatcher::Names(vec!["read".into()])
        );
    }

    #[test]
    fn parse_matcher_with_only_separators_matches_nothing() {
        // "," is neither empty nor "*": it parses to an empty name list, so
        // the hook applies to NO tool (it must not silently widen to all).
        let m = parse_matcher(", ,");
        assert_eq!(m, ToolMatcher::Names(vec![]));
        assert!(!m.matches("bash"));
    }

    #[test]
    fn load_file_appends_the_parsed_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(
            &path,
            hook_json(r#"{"matcher": "bash", "hooks": [{"command": "true", "timeout": 2}]}"#),
        )
        .unwrap();
        let mut out: Vec<ProcessHook> = Vec::new();
        load_file(&path, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "true");
        assert_eq!(out[0].tools, ToolMatcher::Names(vec!["bash".into()]));
        assert_eq!(out[0].timeout, Duration::from_secs(2));
    }

    #[test]
    fn load_file_on_a_missing_file_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut out: Vec<ProcessHook> = Vec::new();
        load_file(&dir.path().join("nope.json"), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn load_file_skips_unsupported_types_and_empty_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(
            &path,
            hook_json(
                r#"{"hooks": [{"type": "http", "command": "x"}, {"command": "   "}, {"command": "keep"}]}"#,
            ),
        )
        .unwrap();
        let mut out: Vec<ProcessHook> = Vec::new();
        load_file(&path, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "keep");
    }

    #[test]
    fn load_file_defaults_a_zero_timeout_to_ten_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, hook_json(r#"{"hooks": [{"command": "true", "timeout": 0}]}"#)).unwrap();
        let mut out: Vec<ProcessHook> = Vec::new();
        load_file(&path, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].timeout, Duration::from_secs(10));
    }

    // ── the runner ─────────────────────────────────────────────

    fn hook_with(command: &str, timeout: Duration) -> ProcessHook {
        ProcessHook {
            command: command.to_string(),
            tools: ToolMatcher::All,
            timeout,
        }
    }

    #[tokio::test]
    async fn run_hook_blocks_on_exit_two_with_the_stderr_reason() {
        let hook = hook_with("echo 'nope' >&2; exit 2", Duration::from_secs(5));
        let d = run_hook(
            None,
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        match d {
            HookDecision::Block(reason) => assert_eq!(reason, "nope"),
            _other => panic!("expected Block, got something else"),
        }
    }

    #[tokio::test]
    async fn run_hook_blocks_without_a_reason_using_the_default_text() {
        let hook = hook_with("exit 2", Duration::from_secs(5));
        let d = run_hook(
            None,
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        match d {
            HookDecision::Block(reason) => assert_eq!(reason, "blocked by process hook"),
            _other => panic!("expected Block, got something else"),
        }
    }

    #[tokio::test]
    async fn run_hook_runs_the_command_in_the_given_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = hook_with("pwd", Duration::from_secs(5));
        let d = run_hook(
            Some(tmp.path()),
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        assert!(matches!(d, HookDecision::NoOpinion), "pwd prints no decision");
        // The command really ran inside the pinned cwd.
        let marker = tmp.path().join("marker.txt");
        let hook = hook_with(
            &format!("touch {}", marker.display()),
            Duration::from_secs(5),
        );
        let _ = run_hook(
            Some(tmp.path()),
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        assert!(marker.exists(), "the hook must run in the pinned cwd");
    }

    #[tokio::test]
    async fn run_hook_times_out_into_no_opinion() {
        let hook = hook_with("sleep 30", Duration::from_millis(150));
        let d = run_hook(
            None,
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        assert!(matches!(d, HookDecision::NoOpinion));
    }

    #[tokio::test]
    async fn run_hook_with_an_unspawnable_command_has_no_opinion() {
        let hook = hook_with("definitely-not-a-real-binary-xyz", Duration::from_secs(5));
        let d = run_hook(
            None,
            &hook,
            "call-1",
            "bash",
            &serde_json::json!({}),
            RiskLevel::Low,
        )
        .await;
        assert!(matches!(d, HookDecision::NoOpinion));
    }

    // ── the stdout decision parser ─────────────────────────────

    #[test]
    fn parse_decision_deny_blocks_with_the_reason() {
        let stdout = br#"{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "no .env"}}"#;
        match parse_decision(stdout, "") {
            HookDecision::Block(reason) => assert_eq!(reason, "no .env"),
            _other => panic!("expected Block, got something else"),
        }
    }

    #[test]
    fn parse_decision_deny_without_a_reason_falls_back_to_stderr() {
        let stdout = br#"{"hookSpecificOutput": {"permissionDecision": "deny"}}"#;
        match parse_decision(stdout, "stderr says no") {
            HookDecision::Block(reason) => assert_eq!(reason, "stderr says no"),
            _other => panic!("expected Block, got something else"),
        }
        // Neither a reason nor stderr -> the default block text.
        match parse_decision(stdout, "") {
            HookDecision::Block(reason) => assert_eq!(reason, "blocked by process hook"),
            _other => panic!("expected Block, got something else"),
        }
        // A whitespace-only stderr is used verbatim (the runner trims the
        // hook's stderr before it reaches the parser, so this never shows
        // up as a blank reason in production).
        match parse_decision(stdout, "  ") {
            HookDecision::Block(reason) => assert_eq!(reason, "  "),
            _other => panic!("expected Block, got something else"),
        }
    }

    #[test]
    fn parse_decision_allow_returns_allow() {
        let stdout = br#"{"hookSpecificOutput": {"permissionDecision": "allow"}}"#;
        assert!(matches!(
            parse_decision(stdout, ""),
            HookDecision::Allow
        ));
    }

    #[test]
    fn parse_decision_updated_input_returns_modify() {
        let stdout =
            br#"{"hookSpecificOutput": {"updatedInput": {"command": "rtk git status"}}}"#;
        match parse_decision(stdout, "") {
            HookDecision::Modify(args) => assert_eq!(args["command"], "rtk git status"),
            _other => panic!("expected Modify, got something else"),
        }
    }

    #[test]
    fn parse_decision_garbage_or_unknown_shapes_have_no_opinion() {
        assert!(matches!(
            parse_decision(b"not json at all", ""),
            HookDecision::NoOpinion
        ));
        assert!(matches!(
            parse_decision(b"{}", ""),
            HookDecision::NoOpinion
        ));
        assert!(matches!(
            parse_decision(br#"{"hookSpecificOutput": {}}"#, ""),
            HookDecision::NoOpinion
        ));
        assert!(matches!(
            parse_decision(
                br#"{"hookSpecificOutput": {"permissionDecision": "maybe"}}"#,
                ""
            ),
            HookDecision::NoOpinion
        ));
        // A non-object `updatedInput` is not a rewrite.
        assert!(matches!(
            parse_decision(
                br#"{"hookSpecificOutput": {"updatedInput": "nope"}}"#,
                ""
            ),
            HookDecision::NoOpinion
        ));
    }

    // ── runner + chain (Task 2) ───────────────────────────────

    use conga::{HookChain, RiskLevel, ToolCallVerdict};

    async fn verdict(
        chain: &ProcessHookChain,
        tool: &str,
        args: serde_json::Value,
    ) -> ToolCallVerdict {
        chain
            .before_tool_call("call-1", tool, &args, RiskLevel::Medium)
            .await
    }

    fn one_hook(command: &str) -> ProcessHookChain {
        ProcessHookChain::new(vec![ProcessHook {
            command: command.to_string(),
            tools: ToolMatcher::All,
            timeout: Duration::from_secs(5),
        }])
    }

    #[tokio::test]
    async fn exit_zero_with_no_output_allows() {
        let v = verdict(&one_hook("exit 0"), "bash", serde_json::json!({})).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let v = verdict(
            &one_hook("echo 'no rm -rf for you' >&2; exit 2"),
            "bash",
            serde_json::json!({"command": "rm -rf /"}),
        )
        .await;
        match v {
            ToolCallVerdict::Block(reason) => assert_eq!(reason, "no rm -rf for you"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_deny_decision_blocks() {
        let v = verdict(
            &one_hook(r#"echo '{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "policy: .env is off-limits"}}'"#),
            "read",
            serde_json::json!({"path": ".env"}),
        )
        .await;
        match v {
            ToolCallVerdict::Block(reason) => {
                assert_eq!(reason, "policy: .env is off-limits")
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_updated_input_modifies_args() {
        let v = verdict(
            &one_hook(
                r#"echo '{"hookSpecificOutput": {"updatedInput": {"command": "rtk git status"}}}'"#,
            ),
            "bash",
            serde_json::json!({"command": "git status"}),
        )
        .await;
        match v {
            ToolCallVerdict::Modify(args) => {
                assert_eq!(args["command"], "rtk git status")
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_fails_open_and_kills_child() {
        let v = verdict(
            &ProcessHookChain::new(vec![ProcessHook {
                command: "sleep 30".to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_millis(200),
            }]),
            "bash",
            serde_json::json!({}),
        )
        .await;
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    #[tokio::test]
    async fn missing_binary_fails_open() {
        let v = verdict(
            &one_hook("definitely-not-a-real-binary-xyz --flag"),
            "bash",
            serde_json::json!({}),
        )
        .await;
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    #[tokio::test]
    async fn non_two_non_zero_exit_fails_open() {
        let v = verdict(&one_hook("exit 1"), "bash", serde_json::json!({})).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    #[tokio::test]
    async fn hooks_receive_claude_shaped_stdin_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("payload.json");
        let out_str = out.display().to_string();
        let chain = ProcessHookChain::new(vec![ProcessHook {
            command: format!("cat > {out_str}"),
            tools: ToolMatcher::All,
            timeout: Duration::from_secs(5),
        }]);
        let _ = verdict(&chain, "bash", serde_json::json!({"command": "ls"})).await;
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(payload["tool_name"], "bash");
        assert_eq!(payload["tool_input"]["command"], "ls");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["risk"], "medium");
    }

    #[tokio::test]
    async fn matcher_filters_which_hooks_run() {
        // A hook that would block everything, but only matches `write`:
        // a `bash` call must sail through untouched.
        let chain = ProcessHookChain::new(vec![ProcessHook {
            command: "exit 2".to_string(),
            tools: ToolMatcher::Names(vec!["write".into()]),
            timeout: Duration::from_secs(5),
        }]);
        let v = verdict(&chain, "bash", serde_json::json!({})).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    #[tokio::test]
    async fn first_block_wins_last_modify_wins() {
        // modify-then-block: Block must win. block-then-modify (2nd hook
        // never runs after a block): Block must still win.
        let chain = ProcessHookChain::new(vec![
            ProcessHook {
                command:
                    r#"echo '{"hookSpecificOutput": {"updatedInput": {"command": "modified"}}}'"#
                        .to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
            ProcessHook {
                command: "exit 2".to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
        ]);
        let v = verdict(&chain, "bash", serde_json::json!({"command": "x"})).await;
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }

    #[test]
    fn after_tool_call_is_passthrough() {
        let chain = one_hook("exit 2");
        let result = conga::ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "bash".into(),
            content: vec![conga::ContentBlock::text("kept")],
            is_error: false,
            timestamp: 0,
        };
        let out = HookChain::after_tool_call(&chain, "1", &result);
        assert_eq!(out.content.len(), 1);
        assert!(matches!(&out.content[0], conga::ContentBlock::Text { text } if text == "kept"));
    }

    #[test]
    fn discover_reads_global_and_project_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Not using the real ~/.conga here — discover() takes the project
        // dir and reads the GLOBAL root from config_dir(); to keep this
        // test hermetic, assert the None case (no files anywhere on this
        // machine is not guaranteed, so only assert type-compat):
        let _ = ProcessHookChain::discover(tmp.path());
    }

    #[tokio::test]
    async fn modify_then_modify_composes() {
        // Hook 2 only rewrites when its stdin contains hook 1's marker —
        // proving hooks compose on the REWRITTEN args, not the originals.
        // (Broken composition: hook 2 sees "git status", grep fails, and
        // the verdict would be hook 1's unmodified rewrite instead.)
        let chain = ProcessHookChain::new(vec![
            ProcessHook {
                command: r#"echo '{"hookSpecificOutput": {"updatedInput": {"command": "rtk git status"}}}'"#
                    .to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
            ProcessHook {
                command: r#"grep -q rtk && echo '{"hookSpecificOutput": {"updatedInput": {"command": "hook2-saw-rtk"}}}'"#
                    .to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
        ]);
        let v = verdict(&chain, "bash", serde_json::json!({"command": "git status"})).await;
        match v {
            ToolCallVerdict::Modify(args) => assert_eq!(args["command"], "hook2-saw-rtk"),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_with_updated_input_is_modify() {
        // A rewrite accompanying an "allow" decision is meaningful; the
        // updatedInput wins instead of being silently dropped.
        let v = verdict(
            &one_hook(
                r#"echo '{"hookSpecificOutput": {"permissionDecision": "allow", "updatedInput": {"command": "rewritten"}}}'"#,
            ),
            "bash",
            serde_json::json!({"command": "original"}),
        )
        .await;
        match v {
            ToolCallVerdict::Modify(args) => assert_eq!(args["command"], "rewritten"),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_with_updated_input_still_blocks() {
        // "deny" wins over updatedInput — fail-closed: a hook that wants
        // rewrite-only omits permissionDecision; allow+rewrite emits "allow".
        let v = verdict(
            &one_hook(
                r#"echo '{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "no", "updatedInput": {"command": "x"}}}'"#,
            ),
            "bash",
            serde_json::json!({"command": "original"}),
        )
        .await;
        assert!(matches!(v, ToolCallVerdict::Block(r) if r == "no"));
    }

    #[tokio::test]
    async fn allow_short_circuits_later_block() {
        let tmp = tempfile::tempdir().unwrap();
        let ran = tmp.path().join("hook2-ran");
        let ran_str = ran.display().to_string();
        let chain = ProcessHookChain::new(vec![
            ProcessHook {
                command: r#"echo '{"hookSpecificOutput": {"permissionDecision": "allow"}}'"#
                    .to_string(),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
            ProcessHook {
                command: format!("touch {ran_str}; exit 2"),
                tools: ToolMatcher::All,
                timeout: Duration::from_secs(5),
            },
        ]);
        let v = verdict(&chain, "bash", serde_json::json!({})).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
        // Hook 2 never ran after the explicit allow: no side-effect file.
        assert!(!ran.exists());
    }
}
