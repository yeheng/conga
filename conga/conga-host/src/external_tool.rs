//! External tools over stdio JSONL (one process, many tools).
//!
//! Protocol (one JSON object per line):
//! ```text
//! → {"op":"list"}
//! ← {"tools":[{"name":"...","description":"...","parameters":{...},"risk":"low"}]}
//! → {"op":"call","id":"...","name":"...","args":{...}}
//! ← {"id":"...","content":[{"type":"text","text":"..."}],"is_error":false}
//! ```
//!
//! Host owns the child; builds `ToolDefinition`s whose `execute` talks to it.
//! No ExtensionApi over the wire. Reload = drop + re-spawn.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use conga::{ContentBlock, RiskLevel, ToolDefinition, ToolError, ToolResult};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ExternalToolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("timeout")]
    Timeout,
    #[error("empty CONGA_EXTERNAL_TOOLS")]
    Empty,
}

#[derive(Debug, Clone, Deserialize)]
struct ListedTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "empty_object")]
    parameters: serde_json::Value,
    #[serde(default)]
    label: Option<String>,
    /// Self-reported risk: "low" | "medium" | "high" (case-insensitive).
    /// A self-report is NEVER trusted on its own — the process is unvetted
    /// code — see [`effective_risk`].
    #[serde(default)]
    risk: Option<String>,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Map a self-reported risk string to a [`RiskLevel`]. Case-insensitive;
/// anything unrecognized (including `None`) falls back to [`RiskLevel::High`]
/// — the safe default, matching how built-in tools treat unknowns.
fn parse_risk(s: Option<&str>) -> RiskLevel {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("low") => RiskLevel::Low,
        Some("medium") => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// Numeric rank of a level for comparisons (`RiskLevel` carries no ordering).
fn risk_rank(r: RiskLevel) -> u8 {
    match r {
        RiskLevel::Low => 0,
        RiskLevel::Medium => 1,
        RiskLevel::High => 2,
    }
}

/// Effective risk for an external tool: an unvetted subprocess defaults to
/// High. A lower level is honored only when the operator explicitly vouched
/// for the command in `CONGA_EXTERNAL_TOOLS` (`@low` etc.) AND the
/// self-report does not exceed the vouch — vouching `@low` cannot wash a
/// `medium`/`high`/missing report clean.
fn effective_risk(vouch: Option<RiskLevel>, self_reported: RiskLevel) -> RiskLevel {
    match vouch {
        Some(v) if risk_rank(self_reported) <= risk_rank(v) => self_reported,
        _ => RiskLevel::High,
    }
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    tools: Vec<ListedTool>,
}

#[derive(Debug, Serialize)]
struct CallRequest<'a> {
    op: &'static str,
    id: &'a str,
    name: &'a str,
    args: &'a serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CallResponse {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    content: Vec<ContentLine>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct ContentLine {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

struct BridgeInner {
    /// Held so `kill_on_drop` reaps the process when the bridge is dropped.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One long-lived external tool process.
pub struct ExternalToolBridge {
    inner: Mutex<BridgeInner>,
    timeout: Duration,
    /// Operator's risk vouch for this command (`@low` etc. in
    /// `CONGA_EXTERNAL_TOOLS`); `None` = every tool stays High.
    risk_vouch: Option<RiskLevel>,
}

impl ExternalToolBridge {
    /// Spawn `program` with `args`, run `list`, return bridge + tool defs.
    /// No risk vouch: every tool of this command registers as High.
    pub async fn spawn(
        program: &str,
        args: &[&str],
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), ExternalToolError> {
        Self::spawn_with_timeout(program, args, None, DEFAULT_TIMEOUT).await
    }

    pub async fn spawn_with_timeout(
        program: &str,
        args: &[&str],
        risk_vouch: Option<RiskLevel>,
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), ExternalToolError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExternalToolError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExternalToolError::Protocol("no stdout".into()))?;

        let bridge = Arc::new(Self {
            inner: Mutex::new(BridgeInner {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            timeout,
            risk_vouch,
        });

        let listed = bridge.list().await?;
        let tools = listed
            .into_iter()
            .map(|t| bridge.tool_definition(t))
            .collect();
        Ok((bridge, tools))
    }

    async fn list(&self) -> Result<Vec<ListedTool>, ExternalToolError> {
        let line = self.roundtrip(r#"{"op":"list"}"#).await?;
        let resp: ListResponse = serde_json::from_str(&line)?;
        Ok(resp.tools)
    }

    async fn call(
        &self,
        id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResponse, ExternalToolError> {
        let req = CallRequest {
            op: "call",
            id,
            name,
            args,
        };
        let payload = serde_json::to_string(&req)?;
        let line = self.roundtrip(&payload).await?;
        Ok(serde_json::from_str(&line)?)
    }

    async fn roundtrip(&self, request_line: &str) -> Result<String, ExternalToolError> {
        let mut guard = self.inner.lock().await;
        guard.stdin.write_all(request_line.as_bytes()).await?;
        guard.stdin.write_all(b"\n").await?;
        guard.stdin.flush().await?;

        let mut line = String::new();
        let read = guard.stdout.read_line(&mut line);
        match tokio::time::timeout(self.timeout, read).await {
            Ok(Ok(0)) => Err(ExternalToolError::Protocol(
                "external tool closed stdout".into(),
            )),
            Ok(Ok(_)) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                if line.is_empty() {
                    return Err(ExternalToolError::Protocol("empty response line".into()));
                }
                Ok(line)
            }
            Ok(Err(e)) => Err(ExternalToolError::Io(e)),
            Err(_) => Err(ExternalToolError::Timeout),
        }
    }

    fn tool_definition(self: &Arc<Self>, t: ListedTool) -> ToolDefinition {
        let bridge = Arc::clone(self);
        let name = t.name.clone();
        let label = t.label.unwrap_or_else(|| t.name.clone());
        let risk = effective_risk(self.risk_vouch, parse_risk(t.risk.as_deref()));
        ToolDefinition {
            name: t.name,
            label,
            description: t.description,
            parameters: t.parameters,
            risk,
            execute: Arc::new(move |ctx| {
                let bridge = Arc::clone(&bridge);
                let name = name.clone();
                Box::pin(async move {
                    if ctx.aborted() {
                        return Ok(ToolResult::error("aborted"));
                    }
                    match bridge.call(&ctx.tool_call_id, &name, &ctx.args).await {
                        Ok(resp) => {
                            let content: Vec<ContentBlock> = resp
                                .content
                                .into_iter()
                                .filter(|c| c.kind == "text" || c.kind.is_empty())
                                .map(|c| ContentBlock::text(c.text))
                                .collect();
                            let content = if content.is_empty() {
                                vec![ContentBlock::text(String::new())]
                            } else {
                                content
                            };
                            Ok(ToolResult {
                                content,
                                details: serde_json::Value::Null,
                                is_error: resp.is_error,
                            })
                        }
                        Err(e) => Err(ToolError::Message(e.to_string())),
                    }
                })
            }),
        }
    }
}

/// One parsed `CONGA_EXTERNAL_TOOLS` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommand {
    /// Program + args to spawn.
    pub argv: Vec<String>,
    /// Operator's risk vouch from a trailing `@low`/`@medium`/`@high`
    /// token. `None` (the default) keeps every tool of this command at
    /// High — a self-reported risk alone is never trusted.
    pub risk: Option<RiskLevel>,
}

/// Parse `CONGA_EXTERNAL_TOOLS`: comma-separated commands, each split on
/// whitespace into program + args (e.g. `python3 /path/echo.py,./bin/tool`).
///
/// An entry may end with a risk vouch: a trailing `@low`, `@medium`, or
/// `@high` token (e.g. `python3 /path/echo.py @low`) lowers the tools of
/// THAT ONE command below the default High — but only as far as the tool's
/// own self-report (see [`effective_risk`]). A trailing `@`-token naming
/// no known level stays a literal argument.
pub fn commands_from_env() -> Vec<ExternalCommand> {
    commands_from(&|k| std::env::var(k))
}

pub fn commands_from(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
) -> Vec<ExternalCommand> {
    let Ok(raw) = lookup("CONGA_EXTERNAL_TOOLS") else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_entry)
        .filter(|c| !c.argv.is_empty())
        .collect()
}

/// Split one entry into argv + optional trailing `@level` vouch.
fn parse_entry(entry: &str) -> ExternalCommand {
    let mut words = shell_words(entry);
    let risk = words
        .last()
        .and_then(|last| last.strip_prefix('@'))
        .and_then(parse_level);
    if risk.is_some() {
        words.pop();
    }
    ExternalCommand {
        argv: words.into_iter().map(str::to_string).collect(),
        risk,
    }
}

/// Recognized vouch level (`low`/`medium`/`high`, case-insensitive).
fn parse_level(s: &str) -> Option<RiskLevel> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(RiskLevel::Low),
        "medium" => Some(RiskLevel::Medium),
        "high" => Some(RiskLevel::High),
        _ => None,
    }
}

/// Minimal whitespace split (no quotes gymnastics). Good enough for env paths.
fn shell_words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Spawn every command from env/list; collect tools. Failures are returned per command.
pub async fn load_all(
    commands: &[ExternalCommand],
) -> Result<Vec<ToolDefinition>, ExternalToolError> {
    let mut tools = Vec::new();
    for cmd in commands {
        let (program, args) = cmd.argv.split_first().ok_or(ExternalToolError::Empty)?;
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (_bridge, defs) =
            ExternalToolBridge::spawn_with_timeout(program, &arg_refs, cmd.risk, DEFAULT_TIMEOUT)
                .await?;
        // Bridge lives inside each tool's execute Arc; drop tools → kill child.
        tools.extend(defs);
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::ToolCallCtx;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    fn fixture_script() -> String {
        // Resolve relative to this crate's tests — write a temp python helper.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echo_tool.py");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("op") == "list":
        print(json.dumps({
            "tools": [{
                "name": "echo",
                "description": "echo args",
                "risk": "low",
                "parameters": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            }]
        }), flush=True)
    elif msg.get("op") == "call":
        text = (msg.get("args") or {}).get("text", "")
        print(json.dumps({
            "id": msg.get("id", ""),
            "content": [{"type": "text", "text": text}],
            "is_error": False
        }), flush=True)
"#,
        )
        .unwrap();
        // Keep dir alive by leaking path string; on unix python reads file by path.
        // Use into_path to persist for process lifetime of test.
        let kept = dir.keep();
        kept.join("echo_tool.py").to_string_lossy().into_owned()
    }

    /// A python helper that answers every request with the given raw line
    /// (used for the protocol / deserialization error paths).
    fn fixture_raw_response(body: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw_tool.py");
        // A JSON string literal is also a valid Python string literal.
        let literal = serde_json::to_string(body).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"#!/usr/bin/env python3
import sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    print({literal}, flush=True)
"#
            ),
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("raw_tool.py").to_string_lossy().into_owned()
    }

    /// A python helper that never answers — used for the timeout path.
    fn fixture_silent() -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent_tool.py");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, time
time.sleep(30)
"#,
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("silent_tool.py").to_string_lossy().into_owned()
    }

    /// Build a tool-call context for the `execute` closures.
    fn call_ctx(args: serde_json::Value) -> conga::ToolCallCtx {
        conga::ToolCallCtx {
            tool_call_id: "c1".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        }
    }

    #[tokio::test]
    async fn list_and_call_echo() {
        let script = fixture_script();
        let (bridge, tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        // Self-reports "low" but no operator vouch: stays High.
        assert_eq!(tools[0].risk, RiskLevel::High);

        let result = (tools[0].execute)(ToolCallCtx {
            tool_call_id: "c1".into(),
            args: serde_json::json!({"text": "hi"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        })
        .await
        .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!(),
        }
        drop(bridge);
    }

    /// The vouch path: `@low` in CONGA_EXTERNAL_TOOLS + a "low"
    /// self-report → Low. Everything else stays as [`effective_risk`].
    #[tokio::test]
    async fn vouched_low_report_is_low() {
        let script = fixture_script();
        let (_bridge, tools) = ExternalToolBridge::spawn_with_timeout(
            "python3",
            &[&script],
            Some(RiskLevel::Low),
            DEFAULT_TIMEOUT,
        )
        .await
        .expect("spawn");
        assert_eq!(tools[0].risk, RiskLevel::Low);
    }

    #[test]
    fn parse_env_commands() {
        let cmds = commands_from(&|k| {
            if k == "CONGA_EXTERNAL_TOOLS" {
                Ok("python3 /tmp/a.py @low, ./bin/x, ./bin/y @tier".into())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        });
        assert_eq!(cmds.len(), 3);
        assert_eq!(
            cmds[0],
            ExternalCommand {
                argv: vec!["python3".to_string(), "/tmp/a.py".to_string()],
                risk: Some(RiskLevel::Low),
            }
        );
        // No vouch: default High for every tool of this command.
        assert_eq!(
            cmds[1],
            ExternalCommand {
                argv: vec!["./bin/x".to_string()],
                risk: None,
            }
        );
        // Unknown level token is NOT a vouch: stays a literal argument.
        assert_eq!(
            cmds[2],
            ExternalCommand {
                argv: vec!["./bin/y".to_string(), "@tier".to_string()],
                risk: None,
            }
        );
    }

    #[test]
    fn parse_risk_maps_known_levels() {
        assert_eq!(parse_risk(Some("low")), RiskLevel::Low);
        assert_eq!(parse_risk(Some("MEDIUM")), RiskLevel::Medium);
        assert_eq!(parse_risk(Some("High")), RiskLevel::High);
        // Unknown / missing → safe default High.
        assert_eq!(parse_risk(Some("extreme")), RiskLevel::High);
        assert_eq!(parse_risk(None), RiskLevel::High);
    }

    #[test]
    fn effective_risk_requires_vouch_that_covers_the_report() {
        // No vouch: a self-reported "low"/"medium" stays High — the default.
        assert_eq!(effective_risk(None, RiskLevel::Low), RiskLevel::High);
        assert_eq!(effective_risk(None, RiskLevel::Medium), RiskLevel::High);
        // Vouch covers the report → honored.
        assert_eq!(
            effective_risk(Some(RiskLevel::Low), RiskLevel::Low),
            RiskLevel::Low
        );
        assert_eq!(
            effective_risk(Some(RiskLevel::Medium), RiskLevel::Low),
            RiskLevel::Low
        );
        // Report exceeds the vouch → High (a @low vouch washes nothing).
        assert_eq!(
            effective_risk(Some(RiskLevel::Low), RiskLevel::Medium),
            RiskLevel::High
        );
        assert_eq!(
            effective_risk(Some(RiskLevel::Low), RiskLevel::High),
            RiskLevel::High
        );
        // @high = trust the self-report wholesale.
        assert_eq!(
            effective_risk(Some(RiskLevel::High), RiskLevel::Low),
            RiskLevel::Low
        );
    }

    // ── defaults and risk ordering ──

    #[test]
    fn empty_object_is_the_default_parameters_schema() {
        let v = empty_object();
        assert_eq!(v["type"], "object");
        assert_eq!(v["properties"], serde_json::json!({}));
    }

    #[test]
    fn risk_rank_orders_the_three_levels() {
        assert_eq!(risk_rank(RiskLevel::Low), 0);
        assert_eq!(risk_rank(RiskLevel::Medium), 1);
        assert_eq!(risk_rank(RiskLevel::High), 2);
        assert!(risk_rank(RiskLevel::Low) < risk_rank(RiskLevel::Medium));
        assert!(risk_rank(RiskLevel::Medium) < risk_rank(RiskLevel::High));
    }

    // ── the stdio protocol ──

    #[tokio::test]
    async fn list_returns_the_tools_the_process_reports() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        let listed = bridge.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "echo");
        assert_eq!(listed[0].description, "echo args");
        assert_eq!(listed[0].risk.as_deref(), Some("low"));
        assert_eq!(listed[0].parameters["properties"]["text"]["type"], "string");
    }

    #[tokio::test]
    async fn call_sends_the_name_and_args_and_returns_the_response() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        let resp = bridge
            .call("c9", "echo", &serde_json::json!({"text": "hi"}))
            .await
            .expect("call");
        assert_eq!(resp.id, "c9");
        assert!(!resp.is_error);
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0].kind, "text");
        assert_eq!(resp.content[0].text, "hi");
    }

    #[tokio::test]
    async fn roundtrip_writes_one_request_line_and_reads_one_response_line() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        let line = bridge
            .roundtrip(r#"{"op":"list"}"#)
            .await
            .expect("roundtrip");
        assert!(
            !line.contains('\n'),
            "the newline must be stripped: {line:?}"
        );
        assert!(!line.contains('\r'));
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert!(v["tools"].is_array(), "got: {v}");
    }

    #[tokio::test]
    async fn roundtrip_reports_an_empty_response_line() {
        let script = fixture_raw_response("");
        let err = match ExternalToolBridge::spawn("python3", &[&script]).await {
            Err(e) => e,
            Ok(_) => panic!("an empty response line must be a protocol error"),
        };
        match err {
            ExternalToolError::Protocol(msg) => {
                assert!(msg.contains("empty response line"), "got: {msg}")
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_reports_a_malformed_response_as_a_serde_error() {
        let script = fixture_raw_response("not json");
        let err = match ExternalToolBridge::spawn("python3", &[&script]).await {
            Err(e) => e,
            Ok(_) => panic!("a malformed response must fail the load"),
        };
        assert!(
            matches!(err, ExternalToolError::Serde(_)),
            "expected a serde error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_times_out_when_the_process_never_answers() {
        let script = fixture_silent();
        let err = match ExternalToolBridge::spawn_with_timeout(
            "python3",
            &[&script],
            None,
            Duration::from_millis(100),
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("a silent process must time out"),
        };
        assert!(
            matches!(err, ExternalToolError::Timeout),
            "expected a timeout, got {err:?}"
        );
    }

    // ── tool definition mapping ──

    #[tokio::test]
    async fn tool_definition_maps_the_listed_tool_onto_a_conga_tool() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        let listed = ListedTool {
            name: "echo".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
            label: None,
            risk: Some("low".into()),
        };
        let def = bridge.tool_definition(listed);
        assert_eq!(def.name, "echo");
        assert_eq!(def.label, "echo", "the label defaults to the name");
        assert_eq!(def.description, "d");
        assert_eq!(def.parameters["type"], "object");
        // Self-reported "low" without an operator vouch → High.
        assert_eq!(def.risk, RiskLevel::High);
    }

    #[tokio::test]
    async fn tool_definition_keeps_an_explicit_label_and_the_vouched_risk() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn_with_timeout(
            "python3",
            &[&script],
            Some(RiskLevel::Low),
            DEFAULT_TIMEOUT,
        )
        .await
        .expect("spawn");
        let listed = ListedTool {
            name: "echo".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
            label: Some("Echo!".into()),
            risk: Some("low".into()),
        };
        let def = bridge.tool_definition(listed);
        assert_eq!(def.label, "Echo!");
        assert_eq!(def.risk, RiskLevel::Low);
    }

    #[tokio::test]
    async fn tool_definition_execute_short_circuits_when_the_call_is_aborted() {
        let script = fixture_script();
        let (bridge, _tools) = ExternalToolBridge::spawn("python3", &[&script])
            .await
            .expect("spawn");
        let listed = ListedTool {
            name: "echo".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
            label: None,
            risk: Some("low".into()),
        };
        let def = bridge.tool_definition(listed);
        let mut ctx = call_ctx(serde_json::json!({"text": "hi"}));
        ctx.signal = Arc::new(AtomicBool::new(true));
        let result = (def.execute)(ctx).await.unwrap();
        assert!(result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "aborted"),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    // ── env parsing ──

    #[test]
    fn commands_from_env_reads_the_process_environment() {
        let expected = commands_from(&|k| std::env::var(k));
        assert_eq!(commands_from_env(), expected);
    }

    #[test]
    fn parse_entry_extracts_the_trailing_risk_vouch() {
        let e = parse_entry("python3 /tmp/a.py @low");
        assert_eq!(e.argv, vec!["python3".to_string(), "/tmp/a.py".to_string()]);
        assert_eq!(e.risk, Some(RiskLevel::Low));

        let e = parse_entry("./bin/x");
        assert_eq!(e.argv, vec!["./bin/x".to_string()]);
        assert_eq!(e.risk, None);

        // An unknown level token stays a literal argument.
        let e = parse_entry("./bin/y @tier");
        assert_eq!(e.argv, vec!["./bin/y".to_string(), "@tier".to_string()]);
        assert_eq!(e.risk, None);

        // An empty entry yields no argv at all.
        let e = parse_entry("");
        assert!(e.argv.is_empty());
        assert_eq!(e.risk, None);
    }

    #[test]
    fn parse_level_recognizes_only_the_known_levels() {
        assert_eq!(parse_level("low"), Some(RiskLevel::Low));
        assert_eq!(parse_level("MEDIUM"), Some(RiskLevel::Medium));
        assert_eq!(parse_level("High"), Some(RiskLevel::High));
        assert_eq!(parse_level("tier"), None);
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("low medium"), None);
    }

    #[test]
    fn shell_words_splits_on_whitespace_only() {
        assert_eq!(shell_words("a b c"), vec!["a", "b", "c"]);
        assert_eq!(shell_words("  a   b  "), vec!["a", "b"]);
        assert_eq!(shell_words("a\tb"), vec!["a", "b"]);
        assert_eq!(shell_words("a\nb"), vec!["a", "b"]);
        assert!(shell_words("").is_empty());
        assert!(shell_words("   ").is_empty());
        // Quotes are NOT interpreted: the split is whitespace-only, so a
        // space inside quotes still separates words and the quote
        // characters stay in the tokens.
        assert_eq!(shell_words("\"a b\""), vec!["\"a", "b\""]);
        assert_eq!(shell_words("a 'b c'"), vec!["a", "'b", "c'"]);
    }

    // ── bulk loading ──

    #[tokio::test]
    async fn load_all_spawns_every_command_and_collects_the_tools() {
        let script = fixture_script();
        let cmds = vec![ExternalCommand {
            argv: vec!["python3".to_string(), script],
            risk: None,
        }];
        let tools = load_all(&cmds).await.expect("load");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn load_all_rejects_an_empty_argv() {
        let cmds = vec![ExternalCommand {
            argv: Vec::new(),
            risk: None,
        }];
        let err = load_all(&cmds).await.expect_err("must fail");
        assert!(
            matches!(err, ExternalToolError::Empty),
            "expected the Empty error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn load_all_reports_a_spawn_failure() {
        let cmds = vec![ExternalCommand {
            argv: vec!["conga-no-such-external-tool-binary".to_string()],
            risk: None,
        }];
        let err = load_all(&cmds).await.expect_err("must fail");
        assert!(
            matches!(err, ExternalToolError::Io(_)),
            "expected an io error, got {err:?}"
        );
    }
}
