//! JS extension scripts (feature `ext-js`): a QuickJS sandbox so extensions
//! can be written in JavaScript instead of compiled Rust crates.
//!
//! A script sees ONE global object, `conga` (defined by [`PRELUDE`]):
//! - `conga.registerTool(def)`          → a tool the LLM may call
//! - `conga.registerBeforeToolCall(fn)` → sync hook: allow / block / modify
//! - `conga.registerAfterToolCall(fn)`  → sync hook: rewrite a tool result
//!
//! Sandboxing: no module loader, no IO, no timers — plain JS only, plus a
//! memory and stack limit. Everything a script can touch lives in this file.
//! The permission gate is NOT bypassable from here: script tools go through
//! the same `before_tool_call` approval path as every other tool, and hooks
//! registered by a script can only add verdicts, never remove the policy's.
//!
//! Threading: QuickJS contexts are single-threaded and `!Send`, but tools
//! execute on tokio workers. All JS lives on one dedicated thread; requests
//! cross a channel, replies come back on a one-shot channel. Tool calls are
//! dispatched BY NAME — the `ToolFn` closure holds no JS values, so nothing
//! with a `'js` lifetime ever escapes the runtime thread. Script hooks must
//! be pure and fast: `before_tool_call` parks the agent loop on a blocking
//! recv while QuickJS runs the hook.

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use conga::extension::api::{AfterToolCallHandler, BeforeToolCallHandler};
use conga::{
    ContentBlock, ExtensionApiImpl, HookChain, RiskLevel, ToolCallCtx, ToolCallVerdict,
    ToolDefinition, ToolError, ToolResult, ToolResultMessage,
};

/// Heap cap for the script runtime.
const SANDBOX_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Stack cap for the script runtime.
const SANDBOX_STACK_LIMIT: usize = 1024 * 1024;

/// Registered before any user script: a registrations collector and a
/// stderr console. Plain `var`s so the collector arrays are reachable as
/// globals from Rust after all scripts ran.
const PRELUDE: &str = r#"
"use strict";
var __conga_tools = [], __conga_before = [], __conga_after = [];
var conga = {
    registerTool(def) { __conga_tools.push(def); },
    registerBeforeToolCall(fn) { __conga_before.push(fn); },
    registerAfterToolCall(fn) { __conga_after.push(fn); },
};
var console = {
    log(...a) { __conga_print("[ext-js]", ...a); },
    error(...a) { __conga_print("[ext-js error]", ...a); },
};
"#;

/// What the JS thread produces during setup, before it enters its request
/// loop. Everything here is `'static` — JS values stay behind on the thread.
pub struct JsExtensions {
    /// Tools registered by all scripts, in script order.
    pub tools: Vec<ToolDefinition>,
    /// The hook chain wrapping every script hook (empty chain if none).
    pub hooks: Arc<dyn HookChain>,
}

/// Tool-context subset a script's `execute(args, ctx)` receives. Deliberately
/// narrower than [`ToolContext`]: no env (may carry secrets).
struct JsToolCtx {
    cwd: PathBuf,
    session_id: String,
    state_dir: PathBuf,
}

/// One request to the JS runtime thread. Each variant carries its own
/// reply channel so the sender can block on exactly this request.
enum JsRequest {
    CallTool {
        name: String,
        args: serde_json::Value,
        ctx: JsToolCtx,
        reply: mpsc::SyncSender<Result<ToolResult, String>>,
    },
    BeforeHook {
        tool_name: String,
        args: serde_json::Value,
        risk: RiskLevel,
        reply: mpsc::SyncSender<ToolCallVerdict>,
    },
    AfterHook {
        tool_name: String,
        result: ToolResultMessage,
        reply: mpsc::SyncSender<ToolResultMessage>,
    },
}

/// Load JS extension scripts into tools + a hook chain.
///
/// Scripts run in registration order at load time; a script that throws
/// aborts loading with its exception (fail loud at startup, same as a
/// config error).
pub fn load_scripts(paths: &[PathBuf]) -> Result<JsExtensions, String> {
    if paths.is_empty() {
        return Ok(JsExtensions {
            tools: Vec::new(),
            hooks: Arc::new(ExtensionApiImpl::new()),
        });
    }
    let (tx, rx) = mpsc::channel::<JsRequest>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<JsExtensions, String>>();

    let paths = paths.to_vec();
    std::thread::Builder::new()
        .name("conga-ext-js".into())
        .spawn(move || js_thread(paths, tx, rx, ready_tx))
        .map_err(|e| format!("spawn ext-js thread: {e}"))?;

    ready_rx.recv().map_err(|_| "ext-js thread died".to_string())?
}

/// The JS runtime thread: build the sandbox, eval the scripts, report the
/// registrations, then serve tool/hook requests until every sender drops.
fn js_thread(
    paths: Vec<PathBuf>,
    tx: mpsc::Sender<JsRequest>,
    rx: mpsc::Receiver<JsRequest>,
    ready: mpsc::Sender<Result<JsExtensions, String>>,
) {
    let run = || -> Result<(), String> {
        let rt = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(SANDBOX_MEMORY_LIMIT);
        rt.set_max_stack_size(SANDBOX_STACK_LIMIT);
        let ctx = rquickjs::Context::full(&rt).map_err(|e| e.to_string())?;
        ctx.with(|ctx| -> Result<(), String> {
            let globals = ctx.globals();
            globals
                .set(
                    "__conga_print",
                    rquickjs::Function::new(
                        ctx.clone(),
                        |args: rquickjs::function::Rest<rquickjs::Value>| {
                            let parts: Vec<String> = args.iter().map(js_debug).collect();
                            eprintln!("{}", parts.join(" "));
                        },
                    ),
                )
                .map_err(|e| error_string(&ctx, e))?;
            ctx.eval::<(), _>(PRELUDE).map_err(|e| error_string(&ctx, e))?;
            for path in &paths {
                let code = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                ctx.eval::<(), _>(code)
                    .map_err(|e| format!("{}: {}", path.display(), error_string(&ctx, e)))?;
            }
            let registry = Registry::collect(&ctx)?;
            ready
                .send(Ok(build_extensions(&registry, tx.clone())))
                .map_err(|_| "loader dropped".to_string())?;
            serve(&ctx, &registry, &rx);
            Ok(())
        })
        .map_err(|e| e.to_string())
    };
    // A setup failure must reach the loader even when the thread is the only
    // remaining holder of `ready` — send the error, then let the thread end.
    if let Err(e) = run() {
        let _ = ready.send(Err(e));
    }
}

/// The `'js` values collected from script registrations, plus every static
/// metadata field already flattened to owned data. Lives only inside the
/// runtime thread's `ctx.with` scope.
struct Registry<'js> {
    tools: Vec<JsToolDef<'js>>,
    before: Vec<rquickjs::Function<'js>>,
    after: Vec<rquickjs::Function<'js>>,
}

/// A registered tool: static metadata (sent to the loader) + the execute
/// function (stays on the runtime thread).
struct JsToolDef<'js> {
    name: String,
    label: String,
    description: String,
    parameters: serde_json::Value,
    risk: RiskLevel,
    execute: rquickjs::Function<'js>,
}

impl<'js> Registry<'js> {
    fn collect(ctx: &rquickjs::Ctx<'js>) -> Result<Self, String> {
        let globals = ctx.globals();
        let collect_fn_array = |name: &str| -> Result<Vec<rquickjs::Function<'js>>, String> {
            let arr: rquickjs::Array = globals.get(name).map_err(|e| error_string(ctx, e))?;
            let mut out = Vec::new();
            for v in arr.iter::<rquickjs::Function<'js>>() {
                out.push(v.map_err(|e| error_string(ctx, e))?);
            }
            Ok(out)
        };
        let tools_arr: rquickjs::Array = globals
            .get("__conga_tools")
            .map_err(|e| error_string(ctx, e))?;
        let mut tools = Vec::new();
        for (i, def) in tools_arr.iter::<rquickjs::Object<'js>>().enumerate() {
            let def = def.map_err(|e| error_string(ctx, e))?;
            let name: String = def
                .get("name")
                .map_err(|e| format!("tool #{i}: {}", error_string(ctx, e)))?;
            let field = |key: &str| -> Result<String, String> {
                let v: Option<String> = def
                    .get(key)
                    .map_err(|e| format!("tool {name}.{key}: {}", error_string(ctx, e)))?;
                Ok(v.unwrap_or_default())
            };
            let label = field("label")?;
            let description = field("description")?;
            let risk = match field("risk")?.as_str() {
                "low" => RiskLevel::Low,
                "medium" => RiskLevel::Medium,
                // Default High: the grading rule says anything unexpected is
                // gated, never waved through.
                _ => RiskLevel::High,
            };
            let parameters = match def
                .get::<_, Option<rquickjs::Value>>("parameters")
                .map_err(|e| format!("tool {name}: {}", error_string(ctx, e)))?
            {
                Some(v) => js_to_json(&v),
                None => serde_json::json!({"type": "object", "properties": {}}),
            };
            let execute: rquickjs::Function<'js> = def
                .get("execute")
                .map_err(|e| format!("tool {name}: {}", error_string(ctx, e)))?;
            tools.push(JsToolDef {
                name,
                label,
                description,
                parameters,
                risk,
                execute,
            });
        }
        Ok(Self {
            tools,
            before: collect_fn_array("__conga_before")?,
            after: collect_fn_array("__conga_after")?,
        })
    }
}

/// Bridge the collected registrations into `'static` public types: one
/// `ToolDefinition` per script tool (its execute goes through the channel,
/// by name) and hook handlers on the shared chain.
fn build_extensions(registry: &Registry<'_>, tx: mpsc::Sender<JsRequest>) -> JsExtensions {
    let bridge = JsBridge { tx: tx.clone() };
    let mut api = ExtensionApiImpl::new();
    if !registry.before.is_empty() {
        api.before_hooks.push(Box::new(bridge.clone()));
    }
    if !registry.after.is_empty() {
        api.after_hooks.push(Box::new(bridge));
    }
    let tools = registry
        .tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            label: t.label.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
            risk: t.risk,
            execute: make_tool_execute(tx.clone(), t.name.clone()),
        })
        .collect();
    JsExtensions {
        tools,
        hooks: Arc::new(api),
    }
}

/// The `ToolFn` for a script tool: channel the call to the runtime thread
/// by name; block on the reply in the blocking pool, never on a worker.
fn make_tool_execute(tx: mpsc::Sender<JsRequest>, name: String) -> conga::ToolFn {
    Arc::new(move |call: ToolCallCtx| {
        let aborted = call.aborted();
        let tx = tx.clone();
        let name = name.clone();
        Box::pin(async move {
            if aborted {
                return Ok(ToolResult::error("aborted"));
            }
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            tx.send(JsRequest::CallTool {
                name,
                args: call.args,
                ctx: JsToolCtx {
                    cwd: call.ctx.cwd,
                    session_id: call.ctx.session_id,
                    state_dir: call.ctx.state_dir,
                },
                reply: reply_tx,
            })
            .map_err(|_| ToolError::Message("ext-js runtime unavailable".into()))?;
            tokio::task::spawn_blocking(move || reply_rx.recv())
                .await
                .map_err(|e| ToolError::Message(format!("ext-js join: {e}")))?
                .map_err(|_| ToolError::Message("ext-js reply dropped".into()))?
                .map_err(ToolError::Message)
        })
    })
}

/// Sender side used by the `'static` tool closures and hook handlers.
#[derive(Clone)]
struct JsBridge {
    tx: mpsc::Sender<JsRequest>,
}

impl JsBridge {
    fn request<T>(&self, make: impl FnOnce(mpsc::SyncSender<T>) -> JsRequest) -> Option<T> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.tx.send(make(reply_tx)).ok()?;
        reply_rx.recv().ok()
    }
}

impl BeforeToolCallHandler for JsBridge {
    fn call(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) -> ToolCallVerdict {
        self.request(|reply| JsRequest::BeforeHook {
            tool_name: tool_name.to_string(),
            args: args.clone(),
            risk,
            reply,
        })
        // Runtime gone: fail open (the permission policy still applies;
        // script hooks are additive, never load-bearing).
        .unwrap_or(ToolCallVerdict::Allow)
    }
}

impl AfterToolCallHandler for JsBridge {
    fn call(
        &self,
        _tool_call_id: &str,
        result: &ToolResultMessage,
    ) -> Option<ToolResultMessage> {
        self.request(|reply| JsRequest::AfterHook {
            tool_name: result.tool_name.clone(),
            result: result.clone(),
            reply,
        })
    }
}

/// Serve requests until every `JsBridge` drops. Runs inside `ctx.with`, so
/// all JS values here share the context's lifetime.
fn serve<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    rx: &mpsc::Receiver<JsRequest>,
) {
    while let Ok(req) = rx.recv() {
        match req {
            JsRequest::CallTool { name, args, ctx: tool_ctx, reply } => {
                let _ = reply.send(call_tool(ctx, registry, &name, args, tool_ctx));
            }
            JsRequest::BeforeHook { tool_name, args, risk, reply } => {
                let _ = reply.send(run_before_hooks(ctx, registry, &tool_name, args, risk));
            }
            JsRequest::AfterHook { tool_name, result, reply } => {
                let _ = reply.send(run_after_hooks(ctx, registry, &tool_name, result));
            }
        }
    }
}

fn call_tool<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    name: &str,
    args: serde_json::Value,
    tool_ctx: JsToolCtx,
) -> Result<ToolResult, String> {
    let execute = registry
        .tools
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.execute.clone())
        .ok_or_else(|| format!("no such js tool: {name}"))?;
    let js_args = json_to_js(ctx, &args).map_err(|e| error_string(ctx, e))?;
    let obj = rquickjs::Object::new(ctx.clone()).map_err(|e| error_string(ctx, e))?;
    obj.set("cwd", tool_ctx.cwd.to_string_lossy().to_string())
        .map_err(|e| error_string(ctx, e))?;
    obj.set("sessionId", tool_ctx.session_id.clone())
        .map_err(|e| error_string(ctx, e))?;
    obj.set("stateDir", tool_ctx.state_dir.to_string_lossy().to_string())
        .map_err(|e| error_string(ctx, e))?;
    let out: rquickjs::Value = match execute.call((js_args, obj.into_value())) {
        Ok(v) => v,
        // A JS throw is the script reporting an error to the LLM — surface
        // it as an error ToolResult, the same way denied host tools do.
        Err(e) => return Ok(ToolResult::error(error_string(ctx, e))),
    };
    tool_result_from_js(ctx, out)
}

/// `execute` returns a string, or `{ content?, isError?, details? }`.
fn tool_result_from_js(
    ctx: &rquickjs::Ctx<'_>,
    value: rquickjs::Value<'_>,
) -> Result<ToolResult, String> {
    if let Some(s) = value.as_string() {
        let text = s.to_string().map_err(|e| error_string(ctx, e))?;
        return Ok(ToolResult::text(text));
    }
    if value.type_of() == rquickjs::Type::Object {
        let obj = value.as_object().unwrap();
        let content: Option<String> = obj.get("content").unwrap_or(None);
        let is_error: bool = obj.get("isError").unwrap_or(false);
        let details: Option<rquickjs::Value> = obj.get("details").unwrap_or(None);
        return Ok(ToolResult {
            content: vec![ContentBlock::text(content.unwrap_or_default())],
            details: details
                .map(|v| js_to_json(&v))
                .unwrap_or(serde_json::Value::Null),
            is_error,
        });
    }
    // undefined / null / anything else → empty successful result.
    Ok(ToolResult::text(String::new()))
}

fn run_before_hooks<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    tool_name: &str,
    mut args: serde_json::Value,
    risk: RiskLevel,
) -> ToolCallVerdict {
    let mut modified = false;
    for hook in &registry.before {
        let payload = rquickjs::Object::new(ctx.clone()).expect("payload object");
        let _ = payload.set("toolName", tool_name);
        let _ = payload.set("args", json_to_js(ctx, &args).expect("args"));
        let _ = payload.set("risk", risk_name(risk));
        let verdict = match hook.call::<_, rquickjs::Value>((payload.into_value(),)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ext-js] before hook threw: {}", error_string(ctx, e));
                continue;
            }
        };
        if let Some(s) = verdict.as_string() {
            if matches!(s.to_string().as_deref(), Ok("allow")) {
                continue;
            }
        }
        if verdict.type_of() == rquickjs::Type::Object {
            let obj = verdict.as_object().unwrap();
            if let Ok(Some(reason)) = obj.get::<_, Option<String>>("block") {
                return ToolCallVerdict::Block(reason);
            }
            if let Ok(Some(replacement)) = obj.get::<_, Option<rquickjs::Value>>("modify") {
                args = js_to_json(&replacement);
                modified = true;
            }
        }
    }
    if modified {
        ToolCallVerdict::Modify(args)
    } else {
        ToolCallVerdict::Allow
    }
}

fn run_after_hooks<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    tool_name: &str,
    mut result: ToolResultMessage,
) -> ToolResultMessage {
    for hook in &registry.after {
        let text = result
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let payload = rquickjs::Object::new(ctx.clone()).expect("payload object");
        let _ = payload.set("toolName", tool_name);
        let _ = payload.set("text", text);
        let _ = payload.set("isError", result.is_error);
        let rewritten = match hook.call::<_, rquickjs::Value>((payload.into_value(),)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ext-js] after hook threw: {}", error_string(ctx, e));
                continue;
            }
        };
        if rewritten.type_of() != rquickjs::Type::Object {
            continue;
        }
        let obj = rewritten.as_object().unwrap();
        if let Ok(Some(text)) = obj.get::<_, Option<String>>("text") {
            result.content = vec![ContentBlock::text(text)];
        }
        if let Ok(Some(is_error)) = obj.get::<_, Option<bool>>("isError") {
            result.is_error = is_error;
        }
    }
    result
}

fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

/// Turn a JS error into a string, consuming any pending exception (rquickjs
/// requires it before the context is used again).
fn error_string(ctx: &rquickjs::Ctx<'_>, err: rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(ex) = caught.clone().into_exception() {
            let msg = ex.message().unwrap_or_else(|| "unknown error".into());
            if let Some(stack) = ex.stack() {
                return format!("{msg}\n{stack}");
            }
            return msg;
        }
        return format!("{:?}", caught);
    }
    err.to_string()
}

// ── serde_json ↔ QuickJS value conversion (both directions are total) ──

fn json_to_js<'js>(
    ctx: &rquickjs::Ctx<'js>,
    value: &serde_json::Value,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    use rquickjs::IntoJs;
    match value {
        serde_json::Value::Null => Ok(rquickjs::Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => b.into_js(ctx),
        serde_json::Value::Number(n) => n.as_f64().into_js(ctx),
        serde_json::Value::String(s) => s.into_js(ctx),
        serde_json::Value::Array(items) => {
            let arr = rquickjs::Array::new(ctx.clone())?;
            for (i, item) in items.iter().enumerate() {
                arr.set(i, json_to_js(ctx, item)?)?;
            }
            Ok(arr.into_value())
        }
        serde_json::Value::Object(map) => {
            let obj = rquickjs::Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k, json_to_js(ctx, v)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

/// Debug rendering for `console.log`-style output on stderr.
fn js_debug(value: &rquickjs::Value<'_>) -> String {
    match value.as_string() {
        Some(s) => s.to_string().unwrap_or_else(|_| "<string>".into()),
        None => format!("{}", js_to_json(value)),
    }
}

fn js_to_json(value: &rquickjs::Value<'_>) -> serde_json::Value {
    match value.type_of() {
        rquickjs::Type::Bool => serde_json::Value::Bool(value.as_bool().unwrap_or(false)),
        rquickjs::Type::Int => serde_json::Value::from(value.as_int().unwrap_or(0)),
        rquickjs::Type::Float => {
            serde_json::Value::from(value.as_float().unwrap_or(f64::NAN))
        }
        rquickjs::Type::String => serde_json::Value::String(
            value
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_default(),
        ),
        rquickjs::Type::Array => {
            let arr = value.as_array().unwrap();
            let items: Vec<serde_json::Value> = arr.iter::<rquickjs::Value>().flatten().map(|v| js_to_json(&v)).collect();
            serde_json::Value::Array(items)
        }
        rquickjs::Type::Object => {
            let obj = value.as_object().unwrap();
            let mut map = serde_json::Map::new();
            for (k, v) in obj.props::<String, rquickjs::Value>().flatten() {
                map.insert(k, js_to_json(&v));
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    /// The acceptance criterion: a usable tool in ≤ 30 lines of script.
    const HELLO: &str = r#"
conga.registerTool({
    name: "js_hello",
    label: "JS Hello",
    description: "Greet someone.",
    parameters: { type: "object", properties: { name: { type: "string" } } },
    risk: "low",
    execute(args) { return "Hello, " + (args.name ?? "world") + "!"; },
});
"#;

    const BLOCKER: &str = r#"
conga.registerBeforeToolCall(({ toolName }) => {
    if (toolName === "bash") return { block: "no bash in demo" };
    if (toolName === "fetch") return { modify: { url: "https://example.com" } };
});
conga.registerAfterToolCall(({ isError }) => {
    if (isError) return { text: "[redacted error]", isError: true };
});
"#;

    fn write_script(dir: &tempfile::TempDir, name: &str, code: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, code).unwrap();
        path
    }

    fn tool_call_ctx(args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "tc1".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: "/tmp".into(),
                env: HashMap::new(),
                session_id: "sess".into(),
                state_dir: "/tmp/state".into(),
            },
        }
    }

    #[tokio::test]
    async fn registers_and_executes_a_tool() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hello.js", HELLO);
        let ext = load_scripts(&[script]).unwrap();
        assert_eq!(ext.tools.len(), 1);
        let tool = &ext.tools[0];
        assert_eq!(tool.name, "js_hello");
        assert_eq!(tool.risk, RiskLevel::Low);
        assert_eq!(tool.parameters["type"], "object");

        let result = (tool.execute)(tool_call_ctx(serde_json::json!({}))).await.unwrap();
        assert_eq!(result.content[0], ContentBlock::text("Hello, world!"));
        let result = (tool.execute)(tool_call_ctx(serde_json::json!({"name": "conga"})))
            .await
            .unwrap();
        assert_eq!(result.content[0], ContentBlock::text("Hello, conga!"));
    }

    #[tokio::test]
    async fn object_results_and_thrown_errors_map_to_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "shapes.js",
            r#"
conga.registerTool({
    name: "ok_shape", description: "", label: "",
    execute() { return { content: "done", isError: false, details: { n: 2 } }; },
});
conga.registerTool({
    name: "throws", description: "", label: "",
    execute() { throw new Error("boom"); },
});
"#,
        );
        let ext = load_scripts(&[script]).unwrap();
        let by_name = |n: &str| ext.tools.iter().find(|t| t.name == n).unwrap().clone();

        let ok = (by_name("ok_shape").execute)(tool_call_ctx(serde_json::json!({}))).await.unwrap();
        assert_eq!(ok.content[0], ContentBlock::text("done"));
        assert_eq!(ok.details["n"], 2);
        assert!(!ok.is_error);

        let err = (by_name("throws").execute)(tool_call_ctx(serde_json::json!({}))).await.unwrap();
        assert!(err.is_error);
        let ContentBlock::Text { text } = &err.content[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("boom"), "got: {text}");
    }

    #[tokio::test]
    async fn before_hooks_block_and_modify_after_hooks_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hooks.js", BLOCKER);
        let ext = load_scripts(&[script]).unwrap();
        let hooks = ext.hooks;

        // Block wins.
        let verdict = hooks
            .before_tool_call("tc", "bash", &serde_json::json!({"cmd": "ls"}), RiskLevel::High)
            .await;
        match verdict {
            ToolCallVerdict::Block(reason) => assert_eq!(reason, "no bash in demo"),
            other => panic!("expected Block, got {other:?}"),
        }

        // Modify updates the args flowing to the LLM's tool call.
        let verdict = hooks
            .before_tool_call("tc", "fetch", &serde_json::json!({"url": "https://evil.com"}), RiskLevel::Medium)
            .await;
        match verdict {
            ToolCallVerdict::Modify(args) => assert_eq!(args["url"], "https://example.com"),
            other => panic!("expected Modify, got {other:?}"),
        }

        // Other tools pass through.
        let verdict = hooks
            .before_tool_call("tc", "read", &serde_json::json!({}), RiskLevel::Low)
            .await;
        assert!(matches!(verdict, ToolCallVerdict::Allow));

        // After-hook rewrites error results, leaves success alone.
        let make_result = |text: &str, is_error: bool| ToolResultMessage {
            tool_call_id: "tc".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            timestamp: 0,
        };
        let rewritten = hooks.after_tool_call("tc", &make_result("secret", true));
        assert_eq!(rewritten.content[0], ContentBlock::text("[redacted error]"));
        let untouched = hooks.after_tool_call("tc", &make_result("fine", false));
        assert_eq!(untouched.content[0], ContentBlock::text("fine"));
    }

    #[tokio::test]
    async fn a_throwing_script_fails_load_loud() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "bad.js", "throw new Error('nope');");
        let err = match load_scripts(&[script]) {
            Err(e) => e,
            Ok(_) => panic!("a throwing script must fail load"),
        };
        assert!(err.contains("bad.js") && err.contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn sandbox_has_no_loader_or_io() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "escape.js", "require('fs');");
        let err = match load_scripts(&[script]) {
            Err(e) => e,
            Ok(_) => panic!("require must be undefined in the sandbox"),
        };
        assert!(err.contains("require"), "got: {err}");
    }
}
