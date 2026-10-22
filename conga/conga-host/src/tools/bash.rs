//! `bash` tool — run a shell command with a timeout, output truncated.

use std::sync::Arc;
use std::time::Duration;

use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

/// Default command timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        label: "Bash".into(),
        description: "Run a shell command in this session's persistent shell: cd, exported env vars, and activated virtualenvs survive across calls (non-Windows). Set run_in_background to start the command without waiting; its output goes to a log file you can read. Optional timeout in seconds.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "description": "seconds (default 120)" },
                "run_in_background": { "type": "boolean", "description": "start the command detached; output is redirected to a log file under the tool state dir (returned in the result)" }
            },
            "required": ["command"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let command = ctx.args["command"]
        .as_str()
        .ok_or_else(|| conga::error::ToolError::Message("command is required".into()))?;
    let timeout = ctx.args["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS);
    let run_in_background = ctx.args["run_in_background"].as_bool().unwrap_or(false);

    if !cfg!(target_os = "windows") {
        // Persistent shell path: state (cwd, exports) survives across calls.
        // The sandbox confines one-shot spawns; a persistent shell cannot be
        // confined the same way, so an enabled sandbox falls back to the
        // one-shot path below.
        if !super::sandbox::sandbox_enabled(&ctx.ctx.env) {
            let bg_dir = if run_in_background {
                Some(ctx.ctx.state_dir.join("bg"))
            } else {
                None
            };
            let outcome = super::shell::run(
                &ctx.ctx.session_id,
                command,
                Duration::from_secs(timeout),
                &ctx.ctx.cwd,
                &ctx.ctx.env,
                bg_dir.as_deref(),
            )
            .await;
            let text = super::spill_or_truncate(&ctx, &outcome.output);
            let is_error = match outcome.exit_code {
                Some(code) => code != 0,
                // No code = the run did not complete (timeout / shell death).
                None => true,
            };
            return Ok(ToolResult {
                content: vec![ContentBlock::text(text.trim())],
                details: serde_json::json!({
                    "persistent": true,
                    "background": run_in_background,
                    "exit_code": outcome.exit_code,
                }),
                is_error,
            });
        }
    }
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    if super::sandbox::sandbox_enabled(&ctx.ctx.env) {
        if let Err(e) = super::sandbox::confine(&mut cmd, &ctx.ctx.cwd) {
            return Ok(ToolResult::error(e));
        }
    }
    cmd.current_dir(&ctx.ctx.cwd);
    cmd.env_clear();
    // Don't leak conga's own config/secrets (e.g. CONGA_LLM_KEY) into
    // commands the model asks to run.
    cmd.envs(ctx.ctx.env.iter().filter(|(k, _)| !k.starts_with("CONGA_")));
    // A timeout drops the `output()` future mid-wait; without kill_on_drop the
    // spawned shell (and its children) would survive as orphans burning CPU.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(ToolResult::error(format!("failed to spawn: {e}")));
        }
        Err(_) => {
            return Ok(ToolResult::error(format!("timed out after {timeout}s")));
        }
    };

    let stdout = super::spill_or_truncate(&ctx, &String::from_utf8_lossy(&output.stdout));
    let stderr = super::spill_or_truncate(&ctx, &String::from_utf8_lossy(&output.stderr));
    let code = output.status.code().unwrap_or(-1);

    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push_str("\n--- stderr ---\n");
        }
        text.push_str(&stderr);
    }
    text.push_str(&format!("\n[exit {}]", code));

    let is_error = !output.status.success();
    Ok(ToolResult {
        content: vec![ContentBlock::text(text.trim())],
        details: serde_json::json!({"exit_code": code, "persistent": false}),
        is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    /// False = the sandbox cannot confine on this host; the caller must bail.
    /// (macOS-only: every confinement test here is `cfg`-gated the same way.)
    ///
    /// Newer macOS refuses any Seatbelt profile containing a `deny` rule, so
    /// these tests would fail for a platform reason rather than a code
    /// defect. Skipping is correct, but silently skipping would hide a dead
    /// security control — so the reason is always printed, and
    /// `sandbox::tests::seatbelt_usable_matches_direct_sandbox_exec` asserts
    /// the probe itself still agrees with the platform.
    #[cfg(target_os = "macos")]
    fn sandbox_works(name: &str) -> bool {
        match crate::tools::sandbox::seatbelt_usable() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("SKIP {name}: {e}");
                false
            }
        }
    }

    async fn run(args: serde_json::Value, cwd: &std::path::Path) -> ToolResult {
        // Unique session per call: the persistent shell serializes per
        // session id, and parallel tests sharing one id would interleave.
        let session = format!("bash-test-{}", uuid::Uuid::new_v4());
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: std::env::vars().collect(),
                session_id: session,
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    /// Same as [`run`], but keeps the `Err` so the error path can be
    /// asserted on.
    async fn run_raw(
        args: serde_json::Value,
        cwd: &std::path::Path,
    ) -> Result<ToolResult, conga::error::ToolError> {
        let session = format!("bash-test-{}", uuid::Uuid::new_v4());
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: std::env::vars().collect(),
                session_id: session,
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
    }

    #[tokio::test]
    async fn runs_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"command": "echo hello"}), tmp.path()).await;
        assert!(!r.is_error, "stderr was captured");
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("hello"), "got: {text}");
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        // Inner sh: a top-level `exit N` would terminate the persistent
        // shell session itself.
        let cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 3"
        } else {
            "sh -c 'exit 3'"
        };
        let r = run(serde_json::json!({"command": cmd}), tmp.path()).await;
        assert!(r.is_error);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("exit 3") || text.contains("[exit"));
    }

    #[tokio::test]
    async fn does_not_leak_conga_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let mut env = std::collections::HashMap::new();
        env.insert("CONGA_LLM_KEY".to_string(), "sk-secret".to_string());
        env.insert("KEEP_ME".to_string(), "visible".to_string());
        let cmd = if cfg!(target_os = "windows") {
            "echo %CONGA_LLM_KEY%%KEEP_ME%"
        } else {
            "echo $CONGA_LLM_KEY$KEEP_ME"
        };
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"command": cmd}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env,
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(!text.contains("sk-secret"), "leaked secret, got: {text}");
        assert!(text.contains("visible"), "non-secret env dropped: {text}");
    }

    /// Timeout must not just fail fast — it must kill the child. The command
    /// records its shell PID, then sleeps well past the 1s timeout; after the
    /// tool returns, that PID must be gone (kill_on_drop fired when the
    /// `output()` future was dropped at the deadline).
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_process() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let start = std::time::Instant::now();
        let r = run(
            serde_json::json!({
                "command": format!("echo $$ > {}; sleep 30", pidfile.display()),
                "timeout": 1
            }),
            tmp.path(),
        )
        .await;
        assert!(
            start.elapsed().as_secs() < 10,
            "must return at the 1s deadline, not after sleep 30"
        );
        assert!(r.is_error);
        match &r.content[0] {
            ContentBlock::Text { text } => assert!(
                text.contains("timed out") || text.contains("[exit timeout]"),
                "got: {text}"
            ),
            _ => panic!("expected text content"),
        }
        // Give the runtime a moment to reap the killed child before probing.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            !alive,
            "child {pid} survived the timeout — kill_on_drop not effective"
        );
    }

    async fn run_with_env(
        args: serde_json::Value,
        cwd: &std::path::Path,
        env: std::collections::HashMap<String, String>,
    ) -> ToolResult {
        // Unique session per call: parallel tests sharing one persistent
        // shell would interleave each other's output.
        let session = format!("bash-test-{}", uuid::Uuid::new_v4());
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env,
                session_id: session,
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    /// Sandbox lets us write inside cwd but not outside the whitelist (cwd /
    /// TMPDIR / /var/tmp). The "outside" dir must dodge $TMPDIR too — plain
    /// tempdir() lands inside it, and the tmp whitelist is supposed to allow
    /// exactly that. Only meaningful where confinement is real (macOS
    /// seatbelt); Linux lands in Task 3, Windows refuses.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_blocks_writes_outside_cwd() {
        if !sandbox_works("sandbox_blocks_writes_outside_cwd") {
            return;
        }
        let cwd = tempfile::tempdir().unwrap();
        // /tmp (-> /private/tmp) is NOT whitelisted; only /var/tmp is.
        let outside = tempfile::tempdir_in("/tmp").unwrap();
        let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());

        // write inside cwd -> allowed
        let r = run_with_env(
            serde_json::json!({"command": "echo x > inside.txt"}),
            cwd.path(),
            env.clone(),
        )
        .await;
        assert!(!r.is_error, "write in cwd must pass: {:?}", r.details);
        assert!(cwd.path().join("inside.txt").exists());

        // write outside cwd -> refused by the seatbelt profile
        let target = outside.path().join("f.txt");
        let r = run_with_env(
            serde_json::json!({"command": format!("echo x > {}", target.display())}),
            cwd.path(),
            env,
        )
        .await;
        assert!(r.is_error, "write outside cwd must fail");
        assert!(!target.exists(), "sandbox did not contain the write");
    }

    /// With CONGA_SANDBOX=1 the $TMPDIR whitelist must actually work: on macOS
    /// /var and /tmp are symlinks (/private/var, /private/tmp) and Seatbelt
    /// matches by resolved real path, so a literal subpath rule never fires.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_allows_writes_under_tmpdir() {
        if !sandbox_works("sandbox_allows_writes_under_tmpdir") {
            return;
        }
        let cwd = tempfile::tempdir().unwrap();
        let target = std::env::temp_dir().join("conga_sandbox_tmpdir_probe.txt");
        let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());
        let r = run_with_env(
            serde_json::json!({"command": format!("echo x > {}", target.display())}),
            cwd.path(),
            env,
        )
        .await;
        assert!(
            !r.is_error,
            "write under temp_dir() must pass under the sandbox: {:?}",
            r.details
        );
        assert!(target.exists(), "sandbox blocked the TMPDIR write");
        let _ = std::fs::remove_file(&target);
    }

    /// A sandbox that cannot confine must refuse the command outright, not
    /// run it wrapped in a no-op. The old failure mode was exit 71 plus
    /// `sandbox_apply: Operation not permitted` — technically "fail closed"
    /// but it told the operator nothing about their sandbox being dead.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_unavailable_refuses_loudly() {
        if crate::tools::sandbox::seatbelt_usable().is_ok() {
            eprintln!("SKIP sandbox_unavailable_refuses_loudly: confinement works on this host");
            return;
        }
        let cwd = tempfile::tempdir().unwrap();
        let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());
        let r = run_with_env(serde_json::json!({"command": "echo x"}), cwd.path(), env).await;
        assert!(r.is_error, "must refuse to run when confinement is a no-op");
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("sandbox"),
            "the refusal must name the sandbox: {text}"
        );
        assert!(
            !text.contains("Operation not permitted"),
            "must not degrade to a raw sandbox-exec error: {text}"
        );
    }

    /// Sandbox ON + short timeout: sandbox-exec execs the command in place
    /// after applying the profile, so the timeout still kills the child.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_timeout_kills_wrapped_command() {
        if !sandbox_works("sandbox_timeout_kills_wrapped_command") {
            return;
        }
        let cwd = tempfile::tempdir().unwrap();
        let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
        env.insert("CONGA_SANDBOX".to_string(), "1".to_string());
        let start = std::time::Instant::now();
        let r = run_with_env(
            serde_json::json!({"command": "sleep 30", "timeout": 1}),
            cwd.path(),
            env,
        )
        .await;
        assert!(
            start.elapsed().as_secs() < 10,
            "must return at the 1s deadline, not after sleep 30"
        );
        assert!(r.is_error);
        match &r.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("timed out"), "got: {text}"),
            _ => panic!("expected text content"),
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn no_sandbox_flag_no_behavior_change() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("f.txt");
        let mut env: std::collections::HashMap<_, _> = std::env::vars().collect();
        // Seal against the outer runner: `CONGA_SANDBOX=1 cargo test` must
        // not flip this into the sandboxed path and false-fail the write.
        env.remove("CONGA_SANDBOX");
        let r = run_with_env(
            serde_json::json!({"command": format!("echo x > {}", target.display())}),
            cwd.path(),
            env,
        )
        .await;
        assert!(!r.is_error, "sandbox off -> old behavior");
        assert!(target.exists());
    }

    // ---- tool definition / argument handling ----

    #[test]
    fn tool_definition_exposes_command_timeout_and_background() {
        let t = tool();
        assert_eq!(t.name, "bash");
        assert_eq!(t.label, "Bash");
        // A tool that executes arbitrary commands is always High risk.
        assert_eq!(t.risk, conga::types::tool::RiskLevel::High);
        assert!(t.parameters["properties"]["command"]["type"] == "string");
        assert!(t.parameters["properties"]["timeout"]["type"] == "integer");
        assert!(t.parameters["properties"]["run_in_background"]["type"] == "boolean");
        assert_eq!(t.parameters["required"], serde_json::json!(["command"]));
    }

    /// Error path: the command argument is mandatory.
    #[tokio::test]
    async fn requires_a_command() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_raw(serde_json::json!({}), tmp.path())
            .await
            .expect_err("a missing command must fail");
        assert!(err.to_string().contains("command is required"), "{err}");
    }

    /// Error path: an aborted call must not run anything.
    #[tokio::test]
    async fn aborts_on_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"command": "echo should-not-run"}),
            signal: Arc::new(AtomicBool::new(true)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: format!("bash-abort-{}", uuid::Uuid::new_v4()),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();
        assert!(r.is_error);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("aborted"), "{text}");
    }

    /// The exit status is reported both in the output and in `details`.
    #[tokio::test]
    async fn reports_the_exit_code_in_details() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 3"
        } else {
            "sh -c 'exit 3'"
        };
        let r = run(serde_json::json!({"command": cmd}), tmp.path()).await;
        assert!(r.is_error, "a non-zero exit is an error result");
        assert_eq!(r.details["exit_code"], 3);
        assert_eq!(r.details["persistent"], true);
    }

    /// A command with no output still reports its exit status.
    #[tokio::test]
    async fn reports_the_exit_status_for_a_silent_command() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 0"
        } else {
            "true"
        };
        let r = run(serde_json::json!({"command": cmd}), tmp.path()).await;
        assert!(!r.is_error, "{:?}", r.details);
        assert_eq!(r.details["exit_code"], 0);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("[exit 0]"), "{text}");
    }

    /// Quotes and newlines in the command must survive the shell wrapper.
    #[tokio::test]
    async fn handles_quotes_and_newlines_in_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"command": "printf '%s\\n' \"a b\"; printf 'x\\ny\\n'"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error, "{:?}", r.details);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("a b"), "quoted argument lost: {text}");
        assert!(text.contains('x'), "{text}");
        assert!(text.contains('y'), "{text}");
    }

    /// Background mode returns immediately and names the log file the
    /// output is written to.
    #[tokio::test]
    async fn runs_in_background_and_names_the_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"command": "echo backgrounded", "run_in_background": true}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error, "{:?}", r.details);
        assert_eq!(r.details["background"], true);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("background output ->"),
            "the log file must be named: {text}"
        );
        assert!(text.contains("bg-"), "the log file must be named: {text}");
    }
}
